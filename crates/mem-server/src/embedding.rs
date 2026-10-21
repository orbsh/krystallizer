//! The embedding engines a DEPLOYMENT can name (ADR-0003). The built-in model lives
//! in `mem-core`; this module owns the client for an OpenAI-shaped embeddings
//! service, which is the OTHER way to satisfy the same contract: one text in, one
//! unit-length vector at the declared width out.
//!
//! Transport never enters `mem-core`. Its surface is store-and-rank, and the shell
//! that already speaks HTTP to consumers is the one that speaks HTTP to a model.
//!
//! Why the client lives on a THREAD of its own: `reqwest::blocking` builds a runtime
//! internally, and a runtime cannot be dropped from inside another one — called from
//! a tokio worker it panics at teardown. So one dedicated thread owns the client for
//! the process's lifetime, requests arrive over a channel, and the answers come back
//! the same way. Requests are therefore SERIALIZED — which is the store's own
//! discipline anyway (one writer, one lock), not a new bottleneck.

use mem_core::embedding::l2_normalize;
use mem_core::{EmbedError, Embedder};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// How long past the HTTP client's own ceiling to wait before calling the worker
/// dead. Only a hung or panicked thread can reach it.
const ANSWER_SLACK: Duration = Duration::from_secs(5);

/// The remote engine: a URL, a model name, an optional bearer token.
pub struct RemoteEmbedder {
    /// The declared width. Every answer is checked against it — a mismatch is a
    /// different vector space, which the store cannot survive — and it is what the
    /// row's frame declares.
    dims: usize,
    /// `{base-url}/embeddings`, as the client built it (errors quote it).
    url: String,
    /// The client's own per-request ceiling, mirrored here so a dead worker is not
    /// waited on forever.
    timeout: Duration,
    jobs: Sender<Job>,
}

impl std::fmt::Debug for RemoteEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteEmbedder")
            .field("url", &self.url)
            .field("dims", &self.dims)
            .field("timeout", &self.timeout)
            .finish()
    }
}

/// One request as the worker sees it.
struct Job {
    text: String,
    reply: Sender<Result<Vec<f32>, EmbedError>>,
}

impl RemoteEmbedder {
    /// Connect, and PROVE it: the client is built on its own thread and one fixed
    /// text is embedded before this returns, so an unreachable service or a wrong
    /// width is a BOOT failure — the doors must never open onto a store that cannot
    /// embed (the same rule the local model follows through a missing artifact).
    pub fn connect(
        base_url: &str,
        model: &str,
        api_key_env: Option<&str>,
        timeout_ms: u64,
        dims: usize,
    ) -> Result<Self, EmbedError> {
        let url = format!("{}/embeddings", base_url.trim_end_matches('/'));
        let key = match api_key_env {
            Some(name) => Some(secret(name, &url)?),
            None => None,
        };
        let timeout = Duration::from_millis(timeout_ms);
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), EmbedError>>();
        let worker = Worker {
            url: url.clone(),
            model: model.to_string(),
            key,
            timeout,
            dims,
        };
        std::thread::Builder::new()
            .name("krystallizer-embedding".to_string())
            .spawn(move || worker.run(inbox, ready_tx))
            .map_err(|e| EmbedError::Service {
                url: url.clone(),
                detail: format!("no thread for the embedding client: {e}"),
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                return Err(EmbedError::Service {
                    url: url.clone(),
                    detail: "the embedding client thread stopped before it was ready".to_string(),
                })
            }
        }
        let engine = Self {
            dims,
            url,
            timeout,
            jobs,
        };
        // The probe: one real answer, checked for width and shape.
        engine.embed("krystallizer")?;
        Ok(engine)
    }
}

impl Embedder for RemoteEmbedder {
    fn dims(&self) -> usize {
        self.dims
    }

    fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        let (reply, answer) = mpsc::channel();
        self.jobs
            .send(Job {
                text: text.to_string(),
                reply,
            })
            .map_err(|_| EmbedError::Service {
                url: self.url.clone(),
                detail: "the embedding client thread is gone".to_string(),
            })?;
        answer
            .recv_timeout(self.timeout + ANSWER_SLACK)
            .map_err(|_| EmbedError::Service {
                url: self.url.clone(),
                detail: format!(
                    "no answer within {} ms (request timeout or a dead client thread)",
                    (self.timeout + ANSWER_SLACK).as_millis()
                ),
            })?
    }
}

/// The environment variable's VALUE, never the name's absence taken as "no auth":
/// `api-key-env` states a dependency, and a missing dependency is a failure.
fn secret(name: &str, url: &str) -> Result<String, EmbedError> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| EmbedError::Service {
            url: url.to_string(),
            detail: format!("`api-key-env` names {name}, which is not set in this environment"),
        })
}

/// The client, its owner, and the wire shape (OpenAI's, the one every provider and
/// every self-hosted server speaks).
struct Worker {
    url: String,
    model: String,
    key: Option<String>,
    timeout: Duration,
    dims: usize,
}

impl Worker {
    fn run(self, jobs: Receiver<Job>, ready: Sender<Result<(), EmbedError>>) {
        // Built HERE, on this thread: a runtime that dies with its owner is fine on
        // a plain thread and a panic inside tokio.
        let client = match reqwest::blocking::Client::builder()
            .timeout(self.timeout)
            .build()
        {
            Ok(client) => {
                let _ = ready.send(Ok(()));
                client
            }
            Err(e) => {
                let _ = ready.send(Err(EmbedError::Service {
                    url: self.url.clone(),
                    detail: format!("the HTTP client does not build: {e}"),
                }));
                return;
            }
        };
        while let Ok(job) = jobs.recv() {
            let answer = self.request(&client, &job.text);
            // The caller went away (a dropped door): stop taking work.
            if job.reply.send(answer).is_err() {
                break;
            }
        }
    }

    fn request(
        &self,
        client: &reqwest::blocking::Client,
        text: &str,
    ) -> Result<Vec<f32>, EmbedError> {
        let mut call = client
            .post(&self.url)
            .json(&serde_json::json!({ "model": self.model, "input": text }));
        if let Some(key) = &self.key {
            call = call.bearer_auth(key);
        }
        let response = call.send().map_err(|e| self.fail(describe(&e)))?;
        let status = response.status();
        let body = response.text().map_err(|e| self.fail(describe(&e)))?;
        if !status.is_success() {
            return Err(self.fail(format!("HTTP {status}: {}", snippet(&body))));
        }
        let answer: Answer = serde_json::from_str(&body)
            .map_err(|e| self.fail(format!("{e} (body: {})", snippet(&body))))?;
        // One input, so one entry; `index` names which input it belongs to, and a
        // service that answers several is still answered by input 0.
        let mut rows = answer.data;
        let row = match rows.iter().position(|row| row.index == 0) {
            Some(at) => rows.swap_remove(at),
            None => rows.into_iter().next().ok_or_else(|| {
                self.fail(format!("an empty `data` array (body: {})", snippet(&body)))
            })?,
        };
        if row.embedding.len() != self.dims {
            return Err(EmbedError::Width {
                url: self.url.clone(),
                got: row.embedding.len(),
                want: self.dims,
            });
        }
        // Normalized HERE, not trusted: the contract is unit length, and a
        // self-hosted server is not obliged to return it.
        l2_normalize(row.embedding).ok_or_else(|| EmbedError::Degenerate {
            origin: self.url.clone(),
        })
    }

    fn fail(&self, detail: String) -> EmbedError {
        EmbedError::Service {
            url: self.url.clone(),
            detail,
        }
    }
}

/// The documented answer: `{"data": [{"embedding": […], "index": 0}], …}`. Extra
/// fields (`model`, `usage`, …) are the service's business, not ours.
#[derive(serde::Deserialize)]
struct Answer {
    data: Vec<Row>,
}

#[derive(serde::Deserialize)]
struct Row {
    #[serde(default)]
    index: usize,
    embedding: Vec<f32>,
}

/// An error AND its sources, one line: `reqwest`'s own Display stops at "error
/// sending request for url …", and the reason (refused, reset, timed out) is the
/// next link. A transport failure nobody can diagnose is not a message.
fn describe(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// A body's first line, bounded — an error message must stay readable.
fn snippet(body: &str) -> String {
    let line = body.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(200) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}
