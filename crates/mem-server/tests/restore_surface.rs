//! The restore surface (ADR-0026): a lake lands in an EMPTY namespace, all of it
//! or the part a mapping names, and a namespace that already holds anything is
//! refused before a single row is written.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use std::collections::BTreeMap;

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

/// A source service that has published one fact, and the lake directory it wrote.
async fn a_published_fact() -> (tempfile::TempDir, tempfile::TempDir) {
    let data = tempfile::tempdir().unwrap();
    let lake = tempfile::tempdir().unwrap();
    let base = spawn_with_lake(data.path(), lake.path()).await;
    let client = reqwest::Client::new();
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
    assert_eq!(landed[0]["outcome"].as_str(), Some("linked"), "{landed}");

    // Wait for the snapshot that CARRIES the row: the publisher's first pass runs at
    // boot and names an empty table, so "the lake has a file" is not the condition.
    let store: Box<dyn okm_lake::store::LakeStore> =
        okm_lake::store::open(&lake.path().display().to_string(), None).unwrap();
    for _ in 0..150 {
        let rows: usize = okm_lake::delta::files_in_order(&*store, "facts")
            .unwrap_or_default()
            .iter()
            .map(|file| {
                store
                    .get(&format!("facts/{file}"))
                    .ok()
                    .and_then(|bytes| {
                        okm_lake::snapshot::parquet_io::read_batches(bytes.into())
                            .ok()
                            .map(|batches| batches.iter().map(|b| b.num_rows()).sum())
                    })
                    .unwrap_or(0)
            })
            .sum();
        if rows >= 1 {
            return (data, lake);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the lake never carried the published fact");
}

#[tokio::test]
async fn a_full_restore_lands_every_table_in_the_namespaces_it_declares() {
    let (_source, lake) = a_published_fact().await;
    let store: Box<dyn okm_lake::store::LakeStore> =
        okm_lake::store::open(&lake.path().display().to_string(), None).unwrap();
    let mut target = mem_core::service::open_service(tempfile::tempdir().unwrap().path()).unwrap();

    let landed = mem_core::restore::land(&mut target, &*store, None).unwrap();
    let facts = landed
        .iter()
        .find(|entry| entry.table == "facts")
        .expect("the edge table is in the plan");
    assert_eq!(facts.rows, 1, "the one fact: {landed:?}");
    let declared = mem_core::export::CATALOG
        .iter()
        .find(|(name, _)| *name == "facts")
        .map(|(_, ns)| *ns)
        .expect("`facts` is a catalogued table");
    assert_eq!(
        facts.ns, declared,
        "no mapping: the declaration's own namespace"
    );
    // The node table came along, because an edge's endpoints are refs into it.
    assert!(
        landed
            .iter()
            .any(|entry| entry.table == "entities" && entry.rows >= 2),
        "both endpoints restored: {landed:?}"
    );
}

#[tokio::test]
async fn a_mapped_restore_lands_only_the_named_table_in_the_namespace_it_is_given() {
    let (_source, lake) = a_published_fact().await;
    let store: Box<dyn okm_lake::store::LakeStore> =
        okm_lake::store::open(&lake.path().display().to_string(), None).unwrap();
    let mut target = mem_core::service::open_service(tempfile::tempdir().unwrap().path()).unwrap();

    let mut mapping = BTreeMap::new();
    mapping.insert("entities".to_string(), 900u16);
    let landed = mem_core::restore::land(&mut target, &*store, Some(&mapping)).unwrap();
    assert_eq!(
        landed.len(),
        1,
        "one table asked for, one landed: {landed:?}"
    );
    assert_eq!((landed[0].table.as_str(), landed[0].ns), ("entities", 900));

    // The rows are in the namespace the caller created, and the declared one stayed
    // untouched — the point of a mapping (partial import, migration).
    assert!(
        !target.graph.ns_is_empty("entities", 900).unwrap().unwrap(),
        "ns 900 holds the restored nodes"
    );
    assert!(
        target
            .graph
            .ns_is_empty("entities", mem_core::graph::ENTITY_NS)
            .unwrap()
            .unwrap(),
        "the declaration's namespace was not written"
    );
}

#[tokio::test]
async fn a_restore_into_a_non_empty_namespace_is_refused_before_anything_lands() {
    let (_source, lake) = a_published_fact().await;
    let store: Box<dyn okm_lake::store::LakeStore> =
        okm_lake::store::open(&lake.path().display().to_string(), None).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut target = mem_core::service::open_service(dir.path()).unwrap();

    mem_core::restore::land(&mut target, &*store, None).unwrap();
    let again = mem_core::restore::land(&mut target, &*store, None);
    let error = again.expect_err("the second restore must be refused");
    assert!(
        error.contains("not empty"),
        "the refusal names the reason: {error}"
    );
    assert!(error.contains("in ns"), "and the namespace: {error}");
}
