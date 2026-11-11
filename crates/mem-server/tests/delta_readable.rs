//! The lake is readable by a tool that is not k10r (ADR-0014): the service publishes
//! itself into a lake and DuckDB opens that lake through its Delta log (okm ADR-0058),
//! with no krystallizer code anywhere in the read path.
//!
//! This is the claim the format is chosen for — "readable by whoever is handed
//! the file, years later" — so it is asserted against a real reader and real
//! files rather than against the metadata being well-formed.
//!
//! Skipped (not failed) when `duckdb` is missing or its delta extension cannot
//! load: both are properties of the machine, not of the files.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::process::Command;

use mem_server::PutCode;

async fn spawn_with_lake(dir: &std::path::Path, lake: &std::path::Path) -> String {
    let target = lake.display().to_string();
    let router = mem_server::open_with_lake(dir.to_path_buf(), &target, 1).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

/// Rows a warehouse reader sees in one table directory.
///
/// `None` means this machine cannot answer at all (no duckdb, or its delta
/// extension will not load). A reader that RAN and failed is a failure, not a
/// skip: that is the difference this test exists to notice.
fn rows(table_dir: &std::path::Path) -> Option<i64> {
    let sql = format!(
        // The log records paths relative to the table directory, which is what
        // makes a bundle relocatable: the reader is pointed at the directory,
        // wherever it now lives.
        "LOAD delta; SELECT count(*)::BIGINT FROM delta_scan('{}')",
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
            eprintln!("skipped: no delta extension: {err}");
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
        "LOAD delta; SELECT {column} FROM delta_scan('{}') ORDER BY {order}",
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
async fn the_lake_opens_in_a_warehouse_reader_that_never_saw_k10r() {
    let store_dir = tempfile::tempdir().unwrap();
    let lake_dir = tempfile::tempdir().unwrap();
    let base = spawn_with_lake(store_dir.path(), lake_dir.path()).await;
    let client = reqwest::Client::new();

    for ts in [1_000u64, 2_000] {
        let text = format!("line {ts}");
        post_finished(
            &client,
            &base,
            7,
            ts,
            0,
            0,
            mem_core::channel::kind::TEXT,
            &text,
        )
        .await;
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
        .post(format!("{base}/code/37"))
        .json(&PutCode {
            code_id: None,
            language: "nu".into(),
            description: "count rows".into(),
            parameters: "{}".into(),
            source: "1".into(),
        })
        .send()
        .await
        .unwrap();

    // The publisher runs at boot and then every second; a publish is a whole snapshot, so
    // waiting for the LAST of the four tables to carry its rows is waiting for the snapshot
    // that names them all.
    let root = lake_dir.path().to_path_buf();
    let mut caught_up = false;
    for _ in 0..150 {
        let seen = (
            rows(&root.join("facts")),
            rows(&root.join("entities")),
            rows(&root.join("code")),
            rows(&root.join("messages")),
        );
        if seen == (Some(2), Some(4), Some(1), Some(4)) {
            caught_up = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    let Some(facts) = rows(&root.join("facts")) else {
        eprintln!("skipped: no duckdb + delta extension on this machine");
        return;
    };
    assert!(
        caught_up,
        "the lake caught up with the writes (facts = {facts})"
    );
    assert_eq!(facts, 2, "both facts in the reader");
    // THREE facts' entities plus the code row's own VERTEX (k10r ADR-0023: a
    // code row is a node, so facts — and tasks — can point at it).
    assert_eq!(
        rows(&root.join("entities")),
        Some(4),
        "three entity nodes and the code vertex"
    );
    assert_eq!(rows(&root.join("code")), Some(1), "one code row");
    assert_eq!(
        rows(&root.join("messages")),
        Some(4),
        "two messages, each with its turn's closing record"
    );

    // The file carries LABELS: the entity table names its rows without the
    // store's vocabulary tables, which is what makes the backup legible.
    let names = column(&root.join("entities"), "name", "name").unwrap();
    let mut names: Vec<String> = names
        .iter()
        .filter(|name| !name.starts_with("code-"))
        .cloned()
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec!["fjall", "k10r", "okm"],
        "labels, not ids (the code vertex is the row this file is about)"
    );
}

/// One finished exchange over the wire (k10r ADR-0024): open a turn, append the
/// record, close the turn keeping it. A closed turn's log is its records PLUS
/// its closing record — which is why a table's row count is turns + records.
#[allow(clippy::too_many_arguments)]
async fn post_finished(
    client: &reqwest::Client,
    base: &str,
    channel: u64,
    ts_ms: u64,
    sub: u16,
    role: u8,
    kind: u8,
    content: &str,
) -> mem_server::MessageKeyDto {
    let key: mem_server::MessageKeyDto = client
        .post(format!("{base}/channels/{channel}/messages"))
        .json(&mem_server::AppendMessage {
            turn_id: None,
            ts_ms,
            sub,
            role,
            kind,
            content: content.to_string(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let turn = key.turn_id;
    let closed = client
        .post(format!("{base}/channels/{channel}/turns/{turn}/close"))
        .json(&serde_json::json!({
            "ts_ms": ts_ms,
            "sub": sub + 1,
            "role": role,
            "content": "",
            "keep": [{ "ts_ms": ts_ms, "sub": sub }],
        }))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(closed, reqwest::StatusCode::NO_CONTENT, "closing the turn");
    key
}
