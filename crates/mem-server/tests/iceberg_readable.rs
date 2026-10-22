//! The backup is readable by a tool that is not k10r (ADR-0013 §3): the bundle
//! is unpacked and then opened by DuckDB through its Iceberg metadata, with no
//! krystallizer code anywhere in the read path.
//!
//! This is the claim the format is chosen for — "readable by whoever is handed
//! the file, years later" — so it is asserted against a real reader and real
//! files rather than against the metadata being well-formed.
//!
//! Skipped (not failed) when `duckdb` is missing or its iceberg extension cannot
//! load: both are properties of the machine, not of the files.

use std::process::Command;

use mem_server::{AppendMessage, PutCode};

async fn spawn(dir: &std::path::Path) -> String {
    let router = mem_server::open(dir.to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Rows a warehouse reader sees in one table directory.
///
/// `None` means this machine cannot answer at all (no duckdb, or its iceberg
/// extension will not load). A reader that RAN and failed is a failure, not a
/// skip: that is the difference this test exists to notice.
fn rows(table_dir: &std::path::Path) -> Option<i64> {
    let sql = format!(
        // `allow_moved_paths` is what a bundle NEEDS: the metadata was written
        // in the directory the export ran in, and the reader now looks at a
        // different one. Without it DuckDB resolves the recorded paths against
        // the process's cwd and opens nothing.
        "LOAD iceberg; SELECT count(*)::BIGINT FROM iceberg_scan('{}', allow_moved_paths = true)",
        table_dir.display()
    );
    let Ok(out) = Command::new("duckdb")
        .args(["-noheader", "-list", "-c", &sql])
        .output()
    else {
        eprintln!("skipped: no duckdb on this machine");
        return None;
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("Extension") || err.contains("Could not load") {
            eprintln!("skipped: no iceberg extension: {err}");
            return None;
        }
        panic!("duckdb could not read {}: {err}", table_dir.display());
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()?
        .trim()
        .parse()
        .ok()
}

/// The names a column holds, in order — used to prove the file carries LABELS
/// (the store's numbering tables are not needed to read it).
fn column(table_dir: &std::path::Path, column: &str, order: &str) -> Option<Vec<String>> {
    let sql = format!(
        "LOAD iceberg; SELECT {column} FROM iceberg_scan('{}', allow_moved_paths = true) ORDER BY {order}",
        table_dir.display()
    );
    let out = Command::new("duckdb")
        .args(["-noheader", "-list", "-c", &sql])
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!("duckdb: {}", String::from_utf8_lossy(&out.stderr));
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

#[tokio::test]
async fn a_bundle_opens_in_a_warehouse_reader_that_never_saw_k10r() {
    let store_dir = tempfile::tempdir().unwrap();
    let base = spawn(store_dir.path()).await;
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
    let landed = client
        .post(format!("{base}/memories/1"))
        .json(&serde_json::json!([
            ["k10r", "system", {}, "DEPENDS_ON", {}, "okm", "system", {}],
            ["okm", "system", {}, "STORES", {}, "fjall", "system", {}],
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(landed.status(), 200);
    client
        .post(format!("{base}/code"))
        .json(&PutCode {
            language: "nu".into(),
            description: "count rows".into(),
            parameters: "{}".into(),
            source: "1".into(),
        })
        .send()
        .await
        .unwrap();

    let bundle = client
        .get(format!("{base}/export"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let work = tempfile::tempdir().unwrap();
    let archive = work.path().join("backup.tar.zst");
    std::fs::write(&archive, &bundle).unwrap();
    let dest = work.path().join("restored");
    std::fs::create_dir_all(&dest).unwrap();
    let root = mem_core::bundle::unpack(&archive, &dest).unwrap();

    // One directory per table, catalog at the root — the layout a reader is
    // pointed at.
    for table in ["facts", "entities", "code", "messages"] {
        assert!(
            mem_core::bundle::table_file(&root, table).is_file(),
            "{table} exported"
        );
        assert!(
            root.join(table).join("metadata/version-hint.text").is_file(),
            "{table} carries the hint a directory read needs"
        );
    }

    let Some(facts) = rows(&root.join("facts")) else {
        eprintln!("skipped: no duckdb + iceberg extension on this machine");
        return;
    };
    assert_eq!(facts, 2, "both facts in the reader");
    assert_eq!(rows(&root.join("entities")), Some(3), "three entity nodes");
    assert_eq!(rows(&root.join("code")), Some(1), "one code row");
    assert_eq!(rows(&root.join("messages")), Some(2), "two messages");

    // The file carries LABELS: the entity table names its rows without the
    // store's vocabulary tables, which is what makes the backup legible.
    let names = column(&root.join("entities"), "name", "name").unwrap();
    assert_eq!(names, vec!["fjall", "k10r", "okm"], "labels, not ids");
}