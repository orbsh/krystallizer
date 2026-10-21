//! PLAN 1d's gate: a FIXED query set over a small corpus, measuring what the
//! landing actually does — precision@1, precision@3, the score of the true
//! node, and the legal "nothing landed" case for an off-corpus query.
//!
//! This is a BASELINE, not the PG comparison the ADR describes: the legacy
//! pgvector + pdb.bm25 + RRF stack is not runnable on this machine, so the
//! ground truth here is hand-labelled (each query names the node it must
//! reach) and the numbers are recorded in PLAN 1d as the starting point a
//! future comparison is measured against.
//!
//! The thresholds are deliberately set just under the measured values — a
//! gate that fails when the model, the floor or the table changes shape is
//! the point; a gate set at the measurement exactly would be a coin flip.
//!
//! Skips (prints why) when the machine has no embedding artifact, so the
//! default gate stays runnable anywhere.

use mem_core::graph::{GraphStore, FactTuple};
use mem_core::{KrystallizerConfig, LANDING_FLOOR};
use std::path::Path;
use std::sync::Arc;

/// The corpus: `(name, kind)`. Deliberately k10r's own vocabulary — the
/// questions an agent actually asks its memory.
const CORPUS: &[(&str, &str)] = &[
    ("fjall_engine", "system"),
    ("slatedb_engine", "system"),
    ("k10r_core", "system"),
    ("galaxy_repo", "repository"),
    ("okm", "library"),
    ("candle", "library"),
    ("bge_small_zh", "model"),
    ("用户记忆", "concept"),
    ("事实图谱", "concept"),
    ("向量检索", "concept"),
    ("关系数据库", "concept"),
    ("写入放大", "concept"),
    ("冷启动", "concept"),
    ("嵌入模型", "concept"),
];

/// `(query, the node it must land on)`.
const QUERIES: &[(&str, &str)] = &[
    ("fjall 存储引擎怎么样", "fjall_engine"),
    ("slatedb 引擎的键值存储", "slatedb_engine"),
    ("k10r 核心是什么", "k10r_core"),
    ("galaxy 那个仓库", "galaxy_repo"),
    ("okm 这个键值库", "okm"),
    ("candle 推理框架", "candle"),
    ("bge 小模型", "bge_small_zh"),
    ("用户的记忆", "用户记忆"),
    ("记忆图谱", "事实图谱"),
    ("向量检索是怎么做的", "向量检索"),
    ("关系数据库的选型", "关系数据库"),
    ("写入放大的问题", "写入放大"),
    ("冷启动太慢了", "冷启动"),
    ("本地嵌入模型", "嵌入模型"),
];

/// An off-corpus query: nothing should land above the floor.
const OFF_CORPUS: &str = "今天北京天气怎么样，要不要带伞";

fn local_embedder() -> Option<Arc<mem_core::Embedder>> {
    let cfg_path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/krystallizer.kdl");
    let cfg = KrystallizerConfig::load(&cfg_path).ok()?;
    let dir = cfg.embedding.resolved_model_dir().ok()?;
    if !dir.join("model.safetensors").exists() {
        eprintln!("skipping: no embedding model in {}", dir.display());
        return None;
    }
    Some(Arc::new(
        mem_core::Embedder::load(&dir).expect("model loads"),
    ))
}

#[test]
fn recall_gate_on_a_fixed_query_set() {
    let Some(engine) = local_embedder() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap().with_embedder(engine);

    // Ingest the corpus as facts: every node is the TAIL of a fact from a
    // fixed hub, so each node exists as a row (with a vector) and the set is
    // connected the way a graph is.
    for (name, kind) in CORPUS {
        let fact = FactTuple::from_json(&format!(
            r#"["corpus_hub","hub",{{}},"CONTAINS",{{}},"{name}","{kind}",{{}}]"#
        ))
        .expect("fixture tuple decodes");
        graph.ingest(1, &fact).expect("fixture ingests");
    }

    let mut at1 = 0usize;
    let mut at3 = 0usize;
    let mut misses: Vec<String> = Vec::new();
    let mut min_true_score = f32::MAX;
    for (query, expected) in QUERIES {
        let seeds = graph.retrieve(1, query, 3).expect("retrieval runs");
        let names: Vec<&str> = seeds.iter().map(|s| s.name.as_str()).collect();
        if names.first() == Some(expected) {
            at1 += 1;
        }
        if names.contains(expected) {
            at3 += 1;
            if let Some(hit) = seeds.iter().find(|s| s.name == *expected) {
                min_true_score = min_true_score.min(hit.score);
            }
        } else {
            misses.push(format!("{query} -> {names:?}"));
        }
        println!(
            "{:<26} expect {:<14} got {:?}",
            query,
            expected,
            seeds
                .iter()
                .map(|s| format!("{}({:.3})", s.name, s.score))
                .collect::<Vec<_>>()
        );
    }
    let n = QUERIES.len();
    let p1 = at1 as f32 / n as f32;
    let p3 = at3 as f32 / n as f32;
    println!(
        "\nprecision@1 = {at1}/{n} = {p1:.3}\nprecision@3 = {at3}/{n} = {p3:.3}\n\
         lowest true-hit score = {min_true_score:.3} (floor {LANDING_FLOOR})\n\
         misses: {misses:#?}"
    );

    // The gate: true hits must clear the floor (a floor above a true hit's
    // score means the landing refuses real memories), and the measured
    // precision must hold. Values sit just under the recorded baseline.
    assert!(
        min_true_score >= LANDING_FLOOR,
        "a true hit scored {min_true_score:.3}, below the floor {LANDING_FLOOR} — \
         the floor is cutting real memories"
    );
    assert!(p1 >= 0.85, "precision@1 fell to {p1:.3} (baseline 2026-10-19: 0.929)");
    assert!(p3 >= 0.95, "precision@3 fell to {p3:.3} (baseline 2026-10-19: 1.000)");

    // The legal "nothing landed": an off-corpus query lands nothing.
    let nothing = graph.retrieve(1, OFF_CORPUS, 5).expect("retrieval runs");
    assert!(
        nothing.is_empty(),
        "off-corpus query landed {:?} — the floor is too permissive",
        nothing
            .iter()
            .map(|s| format!("{}({:.3})", s.name, s.score))
            .collect::<Vec<_>>()
    );
}