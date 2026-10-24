//! `GET /export/{ns}` — the backup boundary (ADR-0007): one table as a Parquet
//! file, over the same HTTP door the service already has.
//!
//! The assertions read the bytes back with a Parquet reader, not just by
//! looking at a status code: a backup nobody can open is not a backup.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unwrap_in_result, clippy::panic_in_result_fn)]

use mem_server::{AppendMessage, PutCode};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

async fn spawn_server() -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let router = mem_server::open(dir.path().to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://{addr}"), dir)
}

/// Count the rows and columns of a Parquet body — enough to prove the file is
/// readable and carries the table, without pinning okm's column names here.
fn shape(bytes: &[u8]) -> (usize, usize) {
    let reader = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(bytes))
        .expect("the body is a Parquet file")
        .build()
        .expect("the Parquet footer is readable");
    let mut rows = 0;
    let mut columns = 0;
    for batch in reader {
        let batch = batch.expect("a batch decodes");
        rows += batch.num_rows();
        columns = batch.num_columns();
    }
    (rows, columns)
}

#[tokio::test]
async fn a_table_leaves_as_a_parquet_file() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/channels/7/messages"))
        .json(&AppendMessage {
            ts_ms: 1_000,
            sub: 0,
            role: 0,
            kind: mem_core::channel::kind::TEXT,
            content: "a line the log keeps".into(),
        })
        .send()
        .await
        .unwrap();

    let response = client
        .get(format!("{base}/export/messages"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/vnd.apache.parquet")
    );
    assert!(response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .contains("messages.parquet"));
    let body = response.bytes().await.unwrap();
    assert_eq!(&body[..4], b"PAR1", "a Parquet file, header and all");
    assert_eq!(shape(&body).0, 1, "the one message is in the file");
}

/// Every table answers a file — including the fact table, which used to name a gap
/// here — and an unknown name is a 404 that points at the catalog.
#[tokio::test]
async fn every_table_answers_and_an_unknown_name_is_a_404() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/code"))
        .json(&PutCode {
            language: "nu".into(),
            description: "log into a host".into(),
            parameters: "{}".into(),
            source: "ssh $args.host".into(),
        })
        .send()
        .await
        .unwrap();

    let code = client
        .get(format!("{base}/export/code"))
        .send()
        .await
        .unwrap();
    assert_eq!(code.status(), 200);
    let body = code.bytes().await.unwrap();
    assert_eq!(shape(&body).0, 1, "the one code row is in the file");

    // An empty table still exports: an empty table is a fact about the backup.
    for table in [
        "checkpoints",
        "entities",
        "cursors",
        "log",
        "members",
        "facts",
    ] {
        let response = client
            .get(format!("{base}/export/{table}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{table} answers a file");
        assert!(
            response.bytes().await.unwrap().len() > 8,
            "{table} is a Parquet file"
        );
    }

    // Facts export now — as the edge table okm's Graph builds (ADR-0034 §3).
    let facts = client
        .get(format!("{base}/export/facts"))
        .send()
        .await
        .unwrap();
    assert_eq!(facts.status(), 200, "the fact table exports");
    let body = facts.bytes().await.unwrap();
    assert_eq!(
        shape(&body).0,
        0,
        "an empty graph exports an empty edge table"
    );

    let nothing = client
        .get(format!("{base}/export/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(nothing.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_export_is_a_snapshot_and_the_store_keeps_writing() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    for ts in [1_000u64, 2_000] {
        client
            .post(format!("{base}/channels/7/messages"))
            .json(&AppendMessage {
                ts_ms: ts,
                sub: 0,
                role: 0,
                kind: mem_core::channel::kind::TEXT,
                content: format!("line {ts}"),
            })
            .send()
            .await
            .unwrap();
    }
    let first = client
        .get(format!("{base}/export/messages"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(shape(&first).0, 2);

    client
        .post(format!("{base}/channels/7/messages"))
        .json(&AppendMessage {
            ts_ms: 3_000,
            sub: 0,
            role: 0,
            kind: mem_core::channel::kind::TEXT,
            content: "a third line".into(),
        })
        .send()
        .await
        .unwrap();
    let second = client
        .get(format!("{base}/export/messages"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(
        shape(&second).0,
        3,
        "the second export sees the third write"
    );
}

fn read_rows(body: &[u8]) -> arrow::record_batch::RecordBatch {
    ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::copy_from_slice(body))
        .expect("parquet")
        .build()
        .expect("footer")
        .next()
        .expect("one batch")
        .expect("decodes")
}

/// `GET /export` is the BUNDLE: one `tar.zst` holding every table's Parquet file
/// beside its Iceberg metadata, plus the catalog at the root.
#[tokio::test]
async fn the_bundle_carries_every_table_and_its_iceberg_metadata() {
    let (base, _dir) = spawn_server().await;
    let response = reqwest::Client::new()
        .get(format!("{base}/export"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/zstd"),
        "a .tar.zst, not a bare Parquet file"
    );
    let body = response.bytes().await.unwrap();
    assert_eq!(&body[..4], &[0x28, 0xb5, 0x2f, 0xfd], "the zstd magic");

    // Unpack it the way a restore does, then read what a reader outside k10r
    // would read: the catalog, and each table's metadata directory.
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("backup.tar.zst");
    std::fs::write(&archive, &body).unwrap();
    let root = mem_core::bundle::unpack(&archive, dir.path()).unwrap();

    let catalog = std::fs::read(root.join(mem_core::bundle::CATALOG_FILE)).unwrap();
    let batch = read_rows(&catalog);
    let names = batch
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::StringArray>()
        .unwrap();
    let mut got: Vec<&str> = (0..batch.num_rows()).map(|i| names.value(i)).collect();
    got.sort_unstable();
    assert_eq!(
        got,
        vec![
            "checkpoints",
            "code",
            "community_seq",
            "cursors",
            "entities",
            "facts",
            "log",
            "members",
            "messages",
        ],
        "every table the service holds, the community sequence included"
    );

    for table in mem_core::export::CATALOG.iter().map(|(n, _)| *n).chain([
        mem_core::export::LOG_REDUCES,
        mem_core::export::CODE_REDUCES,
    ]) {
        let file = mem_core::bundle::table_file(&root, table);
        assert!(file.exists(), "{table} has a data file");
        let meta = mem_core::bundle::table_dir(&root, table).join("metadata");
        assert!(
            meta.join("v1.metadata.json").exists(),
            "{table} carries Iceberg metadata"
        );
        assert!(
            meta.join("version-hint.text").exists(),
            "{table} is readable as a directory"
        );
    }
}

/// A fact leaves as the edge table okm builds: (table, key) endpoints, the
/// predicate's label, and the assertion's props in the `variant` column.
#[tokio::test]
async fn a_fact_exports_with_endpoints_and_props() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    client
        .post(format!("{base}/memories/1"))
        .json(&serde_json::json!([[
            "k10r", "system", {},
            "DEPENDS_ON", { "why": "the backup reads it" },
            "okm", "system", {}
        ]]))
        .send()
        .await
        .unwrap();

    let body = client
        .get(format!("{base}/export/facts"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let batch = read_rows(&body);
    let schema = batch.schema();
    assert_eq!(batch.num_rows(), 1);

    let column = |name: &str| schema.index_of(name).expect(name);
    let text = |name: &str| {
        batch
            .column(column(name))
            .as_any()
            .downcast_ref::<arrow::array::StringArray>()
            .unwrap()
            .value(0)
            .to_string()
    };
    assert_eq!(text("src_table"), "entities", "the catalog resolved the ns");
    assert_eq!(text("dst_table"), "entities");
    assert_eq!(text("predicate"), "DEPENDS_ON");
    let variant = batch.column(column("variant"));
    assert_eq!(variant.len(), 1);
    assert!(variant.is_valid(0), "the props rode the variant column");
}

/// The derived reduce tables carry the watermarks — the thing a restore must
/// replay before it allows another write.
#[tokio::test]
async fn the_reduce_tables_carry_the_watermarks() {
    let (base, _dir) = spawn_server().await;
    let client = reqwest::Client::new();
    for description in ["first", "second"] {
        client
            .post(format!("{base}/code"))
            .json(&PutCode {
                language: "nu".into(),
                description: description.into(),
                parameters: "{}".into(),
                source: "1".into(),
            })
            .send()
            .await
            .unwrap();
    }

    let body = client
        .get(format!("{base}/export/code__reduces"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let batch = read_rows(&body);
    assert_eq!(batch.num_rows(), 1, "one whole-table group");
    let value = batch
        .column(3)
        .as_any()
        .downcast_ref::<arrow::array::BinaryArray>()
        .unwrap()
        .value(0)
        .to_vec();
    assert_eq!(
        u64::from_be_bytes(value.try_into().unwrap()),
        2,
        "the watermark is the last id issued"
    );
}
