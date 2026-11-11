//! The lake surface (ADR-0014): a service with a `lake` node publishes every table into a
//! Delta table by itself, and the manual Parquet doors are gone — `GET /export`,
//! `GET /export/{table}` and `POST /import` no longer exist.
//!
//! The test drives the real router, writes through the real door, waits for the publisher
//! (its first pass runs at boot), then reads the lake the way any external reader would:
//! the Delta log by name, the Parquet file by path, the rows by column.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use okm_lake::store::{LakeStore, LocalFs};

async fn spawn(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_service_publishes_itself_into_the_lake() {
    let data = tempfile::tempdir().unwrap();
    let lake_dir = tempfile::tempdir().unwrap();
    let target = lake_dir.path().display().to_string();
    // One second: the first pass runs at boot, so the test does not wait for an interval.
    let router = mem_server::open_with_lake(data.path().to_path_buf(), &target, 1).unwrap();
    let base = spawn(router).await;
    let client = reqwest::Client::new();

    // One fact through the memory door: the graph's edge table is the smallest table that
    // proves the whole path (row → Parquet → Delta commit).
    // The canonical 8-slot tuple: `[head name, head kind, head props, relation,
    // relation props, tail name, tail kind, tail props]`.
    let landed: serde_json::Value = client
        .post(format!("{base}/memories/1"))
        .json(&serde_json::json!([[
            "galaxy_repo",
            "repository",
            {},
            "PIVOTED_TO",
            {},
            "k10r_core",
            "system",
            {}
        ]]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        landed[0]["outcome"].as_str(),
        Some("linked"),
        "the memory landed: {landed}"
    );

    let lake = LocalFs::new(lake_dir.path());

    /// The rows one live file of a lake table holds, or `None` while there is no version.
    fn rows_in_live_file(lake: &LocalFs, table: &str) -> Option<(String, usize)> {
        let live = okm_lake::delta::files_in_order(lake, table).ok()?;
        if live.len() != 1 {
            return None;
        }
        let bytes = lake.get(&format!("{table}/{}", live[0])).ok()?;
        assert_eq!(
            &bytes[..4],
            b"PAR1",
            "a real Parquet file, not a truncated object"
        );
        let reader = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
            bytes::Bytes::from(bytes),
        )
        .ok()?;
        let rows: usize = reader
            .build()
            .ok()?
            .map(|batch| batch.ok().map(|b| b.num_rows()).unwrap_or(0))
            .sum();
        Some((live[0].clone(), rows))
    }

    // The publisher runs at boot and then every interval; WHICH pass carries the row is a
    // race, so the test waits for a version that holds it rather than for "a version".
    let mut snapshot: Option<(String, usize)> = None;
    for _ in 0..150 {
        if let Some((name, rows)) = rows_in_live_file(&lake, "facts") {
            if rows >= 1 {
                snapshot = Some((name, rows));
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (first, rows) = snapshot.unwrap_or_else(|| {
        panic!(
            "no facts snapshot with a row reached the lake: {:?}",
            lake.list("").unwrap()
        )
    });
    assert!(
        rows >= 1,
        "the published edge is in the file: {rows} row(s)"
    );

    // The schema says which columns are a row's identity (okm ADR-0058): without it a
    // reader cannot fold the table back to current state. It travels in the version that
    // DECLARES the schema — a replacement version names files, it does not restate it — and
    // a DOCUMENT table carries the mark (the facts table is an edge batch, whose identity
    // is the triple the edge set is keyed by and which this service does not restate).
    let declared = String::from_utf8(
        lake.get("entities/_delta_log/00000000000000000000.json")
            .unwrap(),
    )
    .unwrap();
    assert!(
        declared.contains("okm.key"),
        "the schema marks the key columns: {declared}"
    );
    assert!(
        declared.contains("schemaString"),
        "the log states the schema: {declared}"
    );

    // The doors this change removed are gone, not merely undocumented.
    for gone in ["/export", "/export/facts", "/import"] {
        let status = client
            .get(format!("{base}{gone}"))
            .send()
            .await
            .unwrap()
            .status();
        if gone == "/import" {
            let status = client
                .post(format!("{base}{gone}"))
                .send()
                .await
                .unwrap()
                .status();
            assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{gone} answers 404");
        } else {
            assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{gone} answers 404");
        }
    }

    // A second pass REPLACES the version: still one live file, and it is not the first one.
    let mut replaced = false;
    for _ in 0..150 {
        if let Some((name, _)) = rows_in_live_file(&lake, "facts") {
            if name != first {
                replaced = true;
                assert!(
                    lake.head(&format!("facts/{first}")).unwrap().is_none(),
                    "the replaced file left the store"
                );
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        replaced,
        "the next publish replaced the file, not appended to it"
    );
}
