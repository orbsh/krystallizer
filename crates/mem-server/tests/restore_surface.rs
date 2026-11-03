//! The restore round trip (ADR-0013): a bundle written by one service becomes
//! the contents of another, and the second one behaves as if it had always held
//! the data — including the ids it hands OUT next.
//!
//! Every assertion here is against a service that never saw the writes: the
//! bundle is the only thing that crossed.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_server::PutCode;

async fn spawn(dir: &std::path::Path) -> String {
    let router = mem_server::open(dir.to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn fact(head: &str, rel: &str, tail: &str) -> serde_json::Value {
    serde_json::json!([head, "system", {}, rel, {}, tail, "system", {}])
}

#[tokio::test]
async fn a_bundle_restores_rows_and_the_issuers_that_hand_out_ids() {
    let source_dir = tempfile::tempdir().unwrap();
    let source = spawn(source_dir.path()).await;
    let client = reqwest::Client::new();

    // A store with all three kinds of state: channel rows, a fact graph, code.
    for ts in [1_000u64, 2_000] {
        let text = format!("line {ts}");
        post_finished(&client, &source, 7, ts, 0, 0, mem_core::channel::kind::TEXT, &text).await;
    }
    let landed = client
        .post(format!("{source}/memories/1"))
        .json(&serde_json::json!([
            fact("k10r", "DEPENDS_ON", "okm"),
            fact("okm", "STORES", "fjall"),
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(landed.status(), 200);
    let landed: Vec<serde_json::Value> = landed.json().await.unwrap();
    assert_eq!(landed.len(), 2, "both facts answered");
    for description in ["first", "second"] {
        client
            .post(format!("{source}/code/37"))
            .json(&PutCode {
                code_id: None,
                language: "nu".into(),
                description: description.into(),
                parameters: "{}".into(),
                source: "1".into(),
            })
            .send()
            .await
            .unwrap();
    }

    let bundle = client
        .get(format!("{source}/export"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    // A DIFFERENT service, on a different directory, restored from the bytes.
    let restored_dir = tempfile::tempdir().unwrap();
    let restored = spawn(restored_dir.path()).await;
    let response = client
        .post(format!("{restored}/import"))
        .header("content-type", "application/zstd")
        .body(bundle)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let text = response.text().await.unwrap();
    assert_eq!(status, 200, "{text}");
    let report: std::collections::BTreeMap<String, usize> = serde_json::from_str(&text).unwrap();
    assert_eq!(report["messages"], 4, "two records + two closing records");
    assert_eq!(report["facts"], 2);
    assert_eq!(report["code"], 2);
    // The reduce table is imported LAST, and by then re-folding the restored
    // rows has already rebuilt the watermark — so the push is a no-op (0 rows)
    // rather than a second write. Its presence in the report is what matters:
    // a table whose watermark file is missing is a refused restore.
    assert!(
        report.contains_key("code__reduces"),
        "the derived table travels: {report:?}"
    );

    // Rows are there, read back through the doors.
    let code = client
        .get(format!("{restored}/code/37/1"))
        .send()
        .await
        .unwrap();
    assert_eq!(code.status(), 200, "code id 1 restored");
    assert!(code.text().await.unwrap().contains("first"));
    let code = client
        .get(format!("{restored}/code/37/2"))
        .send()
        .await
        .unwrap();
    assert_eq!(code.status(), 200);
    assert!(code.text().await.unwrap().contains("second"));

    // The entities and the facts came back. Exact-name reads, not a landing:
    // this service is opened WITHOUT an embedding engine (an engine needs a
    // model artifact), and `retrieve` on an engine-less store answers an empty
    // landing BY DESIGN — which would prove nothing about the rows.
    for name in ["k10r", "okm", "fjall"] {
        let entity = client
            .get(format!("{restored}/memories/1/entities/{name}"))
            .send()
            .await
            .unwrap();
        assert_eq!(entity.status(), 200, "the entity `{name}` restored");
    }
    let restored_fact = client
        .get(format!("{restored}/graph/1/facts/1"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        restored_fact.status(),
        200,
        "edge id 1 restored under its own id"
    );
    let restored_fact: serde_json::Value = restored_fact.json().await.unwrap();
    assert_eq!(restored_fact["kind"], "DEPENDS_ON");

    // THE ISSUERS: the restored service must not hand out an id already in use.
    let next_code: serde_json::Value = client
        .post(format!("{restored}/code/37"))
        .json(&PutCode {
            code_id: None,
            language: "nu".into(),
            description: "third".into(),
            parameters: "{}".into(),
            source: "2".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        next_code["code_id"], 3,
        "the id watermark came with the file"
    );

    let next_fact: Vec<serde_json::Value> = client
        .post(format!("{restored}/memories/1"))
        .json(&serde_json::json!([fact("fjall", "BACKS", "okm")]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(next_fact[0]["outcome"], "linked");
    assert_eq!(
        next_fact[0]["edge_id"], 3,
        "the edge sequence came with the edge table"
    );
}

#[tokio::test]
async fn a_body_that_is_not_a_bundle_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let base = spawn(dir.path()).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/import"))
        .header("content-type", "application/zstd")
        .body(b"not a tar.zst".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let text = response.text().await.unwrap();
    assert!(
        text.contains("zstd") || text.contains("archive") || text.contains("tar"),
        "{text}"
    );
}

#[tokio::test]
async fn an_empty_body_is_refused_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let base = spawn(dir.path()).await;
    let response = reqwest::Client::new()
        .post(format!("{base}/import"))
        .body(Vec::<u8>::new())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(response.text().await.unwrap().contains("empty"));
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
    let opened: serde_json::Value = client
        .post(format!("{base}/channels/{channel}/turns"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let turn = opened["turn_id"].as_u64().unwrap();
    let key: mem_server::MessageKeyDto = client
        .post(format!("{base}/channels/{channel}/messages"))
        .json(&mem_server::AppendMessage {
            turn_id: turn,
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
