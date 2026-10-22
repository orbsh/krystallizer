//! The graph view is served BY the memory service (ADR-0007: no CORS on a
//! service that holds user memory), and it draws what `GET /graph/{scope}`
//! answers — so the page, its library and the data come from one origin.

async fn spawn(dir: &std::path::Path) -> String {
    let router = mem_server::open(dir.to_path_buf()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn the_page_and_its_library_come_from_the_same_origin_as_the_data() {
    let dir = tempfile::tempdir().unwrap();
    let base = spawn(dir.path()).await;
    let client = reqwest::Client::new();

    let page = client.get(format!("{base}/viz")).send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(
        page.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/html; charset=utf-8")
    );
    let html = page.text().await.unwrap();
    // The library arrives from this service, not from a CDN.
    assert!(html.contains(r#"src="/viz/vendor/3d-force-graph.js""#));
    // And the data door is relative, so it stays same-origin.
    assert!(html.contains("fetch(`/graph/${scope}`)"));
    assert!(!html.contains("http://") && !html.contains("https://"));

    let library = client
        .get(format!("{base}/viz/vendor/3d-force-graph.js"))
        .send()
        .await
        .unwrap();
    assert_eq!(library.status(), 200);
    assert_eq!(
        library
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/javascript; charset=utf-8")
    );
    let bytes = library.bytes().await.unwrap();
    assert!(bytes.len() > 100_000, "the vendored library, not a stub");
}

#[tokio::test]
async fn the_graph_door_answers_the_shape_the_page_draws() {
    let dir = tempfile::tempdir().unwrap();
    let base = spawn(dir.path()).await;
    let client = reqwest::Client::new();

    let landed = client
        .post(format!("{base}/memories/1"))
        .json(&serde_json::json!([
            ["k10r", "system", {}, "DEPENDS_ON", {}, "okm", "system", {}],
        ]))
        .send()
        .await
        .unwrap();
    assert_eq!(landed.status(), 200);

    let graph: serde_json::Value = client
        .get(format!("{base}/graph/1"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let nodes = graph["nodes"].as_array().unwrap();
    assert!(nodes.len() >= 2, "both endpoints are nodes: {graph}");
    // The three fields the view colours and labels by.
    for node in nodes {
        assert!(node.get("id").is_some());
        assert!(node.get("kind").is_some());
        assert!(
            node.get("community_id").is_some(),
            "the colour source travels: {node}"
        );
    }
    let links = graph["links"].as_array().unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["kind"], "DEPENDS_ON");
    // Endpoints are NODE IDs here; the page resolves them before handing the
    // array to the physics (a string endpoint freezes the render loop).
    let ids: Vec<i64> = nodes
        .iter()
        .map(|n| n["id"].as_i64().unwrap())
        .collect();
    for end in ["source", "target"] {
        assert!(ids.contains(&links[0][end].as_i64().unwrap()), "{end} is a node");
    }
}