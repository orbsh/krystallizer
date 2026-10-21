//! `GET /export/{ns}` — the backup boundary (ADR-0007): one table as a Parquet
//! file, over the same HTTP door the service already has.
//!
//! The assertions read the bytes back with a Parquet reader, not just by
//! looking at a status code: a backup nobody can open is not a backup.

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
        .get(format!("{base}/export/33"))
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
        .contains("ns33.parquet"));
    let body = response.bytes().await.unwrap();
    assert_eq!(&body[..4], b"PAR1", "a Parquet file, header and all");
    assert_eq!(shape(&body).0, 1, "the one message is in the file");
}

#[tokio::test]
async fn every_document_table_answers_and_the_fact_table_names_its_gap() {
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

    let code = client.get(format!("{base}/export/35")).send().await.unwrap();
    assert_eq!(code.status(), 200);
    let body = code.bytes().await.unwrap();
    assert_eq!(shape(&body).0, 1, "the one code row is in the file");

    // An empty table still exports: an empty table is a fact about the backup.
    for ns in [34u16, 36, 39, 41, 42] {
        let response = client
            .get(format!("{base}/export/{ns}"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "ns {ns} answers");
        assert!(response.bytes().await.unwrap().len() > 8, "ns {ns} is a file");
    }

    // Facts are GraphEncode entries: no columnar form until okm's Graph exports.
    let facts = client
        .get(format!("{base}/export/37"))
        .send()
        .await
        .unwrap();
    assert_eq!(facts.status(), reqwest::StatusCode::NOT_IMPLEMENTED);
    let reason = facts.text().await.unwrap();
    assert!(reason.contains("okm"), "the gap names where it lives: {reason}");

    let nothing = client.get(format!("{base}/export/9")).send().await.unwrap();
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
        .get(format!("{base}/export/33"))
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
        .get(format!("{base}/export/33"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(shape(&second).0, 3, "the second export sees the third write");
}
