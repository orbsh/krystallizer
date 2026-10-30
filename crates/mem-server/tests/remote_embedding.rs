//! The remote embedding mode (ADR-0003): the vectors are computed on another machine,
//! and the contract does not move — one text in, one unit-length vector at the declared
//! width out.
//!
//! A MOCK service speaks OpenAI's shape (`POST {base-url}/embeddings`), so these tests
//! prove the client's request and the store's use of it without a network or a GPU.
//! What is load-bearing: the route is appended (the file names a deployment, not a
//! route), the bearer token comes from the variable the config names, a wrong width is
//! a BOOT failure (two vector spaces in one table is not survivable), the vector is
//! normalized here rather than trusted, and a fact ingested through the door is
//! retrievable — i.e. the seam really reaches `ingest` and `retrieve`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use mem_core::Embedder as _;
use mem_server::SeedDto;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// One request the mock saw, so the client's shape can be asserted from the other side.
#[derive(Debug, Clone)]
struct Seen {
    path: String,
    authorization: Option<String>,
    model: String,
    input: String,
}

/// What the mock is told to do.
struct Mock {
    seen: Arc<Mutex<Vec<Seen>>>,
    /// The width it answers with.
    dims: usize,
    /// The first component's magnitude: 2.0 proves the client normalizes.
    scale: f32,
    /// Non-200 to make the service fail.
    status: u16,
}

/// Start a service that speaks the documented shape on an OS-picked port.
///
/// The mock runs on its OWN thread with its own runtime, like the machine it stands
/// in for: the client blocks the calling thread while a request is in flight, so a
/// mock sharing the test's runtime competes with the very task that is waiting for it
/// (and loses whenever the worker pool is busy).
fn mock_embeddings(dims: usize, scale: f32, status: u16) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let mock = Arc::new(Mock {
        seen: Arc::new(Mutex::new(Vec::new())),
        dims,
        scale,
        status,
    });
    let seen = mock.seen.clone();
    let (ready, addr) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("mock-embeddings".to_string())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("a runtime for the mock");
            runtime.block_on(async move {
                let app = Router::new()
                    .route("/v1/embeddings", post(answer))
                    .with_state(mock);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                ready.send(listener.local_addr().unwrap()).unwrap();
                axum::serve(listener, app).await.unwrap();
            });
        })
        .expect("a thread for the mock");
    let addr: SocketAddr = addr.recv().expect("the mock is listening");
    (format!("http://{addr}/v1"), seen)
}

async fn answer(
    State(mock): State<Arc<Mock>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    mock.seen.lock().unwrap().push(Seen {
        path: "/v1/embeddings".to_string(),
        authorization: headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string),
        model: body["model"].as_str().unwrap_or_default().to_string(),
        input: body["input"].as_str().unwrap_or_default().to_string(),
    });
    if mock.status != 200 {
        return (
            StatusCode::from_u16(mock.status).unwrap(),
            "the model is loading",
        )
            .into_response();
    }
    // A one-hot vector with a magnitude: unit length only if it is normalized.
    let mut embedding = vec![0.0f32; mock.dims];
    embedding[0] = mock.scale;
    Json(serde_json::json!({
        "object": "list",
        "model": body["model"],
        "data": [{"object": "embedding", "index": 0, "embedding": embedding}],
        "usage": {"prompt_tokens": 1, "total_tokens": 1}
    }))
    .into_response()
}

fn engine(
    base_url: &str,
    api_key_env: Option<&str>,
    dims: usize,
) -> Result<mem_server::embedding::RemoteEmbedder, mem_core::EmbedError> {
    mem_server::embedding::RemoteEmbedder::connect(
        base_url,
        "bge-small-zh-v1.5",
        api_key_env,
        5_000,
        dims,
    )
}

/// The happy path: the route is the client's, the token rides the header, and the
/// answer is unit length at the declared width — normalized HERE, since the mock
/// answers magnitude 2.
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_engine_answers_through_the_openai_shape() {
    let key_var = "KRYSTALLIZER_TEST_EMBEDDING_KEY";
    std::env::set_var(key_var, "test-key");
    let (base, seen) = mock_embeddings(512, 2.0, 200);

    let engine = engine(&base, Some(key_var), 512).expect("the service answers");
    assert_eq!(engine.dims(), 512);
    let vector = engine.embed("记忆图谱").expect("one vector");
    assert_eq!(vector.len(), 512);
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-6, "unit length, got {norm}");

    let seen = seen.lock().unwrap();
    // Two requests: the boot probe, then the real one.
    assert_eq!(seen.len(), 2, "the boot probes once");
    let last = seen.last().unwrap();
    assert_eq!(
        last.path, "/v1/embeddings",
        "the client appends the route to the base URL"
    );
    assert_eq!(last.authorization.as_deref(), Some("Bearer test-key"));
    assert_eq!(last.model, "bge-small-zh-v1.5");
    assert_eq!(last.input, "记忆图谱");
    std::env::remove_var(key_var);
}

/// A service without the key needs no header — the field's absence is the spelling.
#[tokio::test(flavor = "multi_thread")]
async fn a_service_without_a_key_sends_no_authorization() {
    let (base, seen) = mock_embeddings(8, 1.0, 200);
    let engine = engine(&base, None, 8).expect("the service answers");
    engine.embed("x").expect("one vector");
    let seen = seen.lock().unwrap();
    assert!(
        seen.iter().all(|request| request.authorization.is_none()),
        "no header when the config names no variable"
    );
}

/// The wrong width is refused AT BOOT: truncating or padding would mix two vector
/// spaces inside one table, which the store cannot survive.
#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_width_is_a_boot_failure() {
    let (base, _seen) = mock_embeddings(3, 1.0, 200);
    let err = engine(&base, None, 512).expect_err("refused");
    let msg = err.to_string();
    assert!(msg.contains("width 3"), "names what came back: {msg}");
    assert!(msg.contains("512"), "names what was declared: {msg}");
}

/// An unreachable or failing service is a boot failure too, and its own words travel.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_service_is_a_boot_failure() {
    let (base, _seen) = mock_embeddings(512, 1.0, 500);
    let err = engine(&base, None, 512).expect_err("refused");
    let msg = err.to_string();
    assert!(msg.contains("HTTP 500"), "names the status: {msg}");
    assert!(
        msg.contains("the model is loading"),
        "carries the service's own reason: {msg}"
    );
}

/// `api-key-env` states a dependency: an unset variable is named, not silently treated
/// as "no auth".
#[tokio::test(flavor = "multi_thread")]
async fn a_missing_key_variable_is_named() {
    let (base, seen) = mock_embeddings(512, 1.0, 200);
    let err = engine(&base, Some("KRYSTALLIZER_TEST_ABSENT_KEY"), 512).expect_err("refused");
    let msg = err.to_string();
    assert!(
        msg.contains("KRYSTALLIZER_TEST_ABSENT_KEY"),
        "names the variable: {msg}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "nothing was called before the key was resolved"
    );
}

/// The whole way round: a config file in remote mode boots the service, a fact lands
/// through the door, and the same door finds it by vector landing — proof the seam
/// reaches `ingest` and `retrieve` rather than only the client.
#[tokio::test(flavor = "multi_thread")]
async fn a_remote_deployment_ingests_and_lands_a_fact() {
    let (base, _seen) = mock_embeddings(512, 1.0, 200);
    let dir = tempfile::TempDir::new().unwrap();
    let data = dir.path().join("data");
    let config = dir.path().join("krystallizer.kdl");
    std::fs::write(
        &config,
        format!(
            r#"storage {{
    data-dir "{data}"
}}
embedding {{
    mode "remote"
    base-url "{base}"
    model "bge-small-zh-v1.5"
    timeout-ms 5000
    dimensions 512
    metric "cosine"
}}
compression {{
    context-bytes 131072
    budget-percent 50
    cache-ttl-ms 300000
    floor-bytes 2048
}}
logs {{
    max-rows 10000
}}
"#,
            data = data.display(),
            base = base,
        ),
    )
    .unwrap();

    let cfg = mem_core::KrystallizerConfig::load(&config).expect("the config decodes");
    let app = mem_server::open_from_config(&cfg).expect("the service boots");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let base_http = format!("http://{addr}");
    let client = reqwest::Client::new();

    let tuple: serde_json::Value =
        serde_json::from_str(r#"["用户","person",{},"PREFERS",{},"冰美式","coffee",{}]"#).unwrap();
    // The write door is plural: one fact is a one-element array.
    let answer = client
        .post(format!("{base_http}/memories/1"))
        .json(&serde_json::json!([tuple]))
        .send()
        .await
        .unwrap();
    let status = answer.status();
    let body = answer.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "the fact lands: {body}");

    // And so is the landing door: one query is a one-element array, answered by
    // a one-element array of seed lists.
    let search = client
        .post(format!("{base_http}/graph/1/search"))
        .json(&serde_json::json!(["冰美式"]))
        .send()
        .await
        .unwrap();
    let status = search.status();
    let body = search.text().await.unwrap();
    assert!(status.is_success(), "the landing answers: {status} {body}");
    let answers: Vec<Vec<SeedDto>> = serde_json::from_str(&body).unwrap();
    let seeds = answers.into_iter().next().unwrap_or_default();
    assert!(
        !seeds.is_empty(),
        "the node the remote engine embedded is reachable by landing"
    );
    assert!(
        seeds.iter().any(|seed| seed.name == "冰美式"),
        "the ingested entity is among the seeds: {seeds:?}"
    );
}
