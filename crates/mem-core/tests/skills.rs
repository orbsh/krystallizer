//! The skill node's contract (ADR-0015): declaring is idempotent, the read
//! tells absence from a wrong kind, and the resolver answers skills and only
//! skills — with an engine when there is one, by the kind face when there is
//! not (a deployment without a model still knows what it can do).
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unwrap_in_result,
    clippy::panic_in_result_fn
)]

use mem_core::graph::{FactTuple, GraphStore};
use mem_core::skills::{
    SkillDecl, SkillRead, DEFAULT_LIMIT, MAX_LIMIT, PRECONDITIONS_FIELD, SKILL_KIND, STEPS_FIELD,
};

fn decl(name: &str, content: &str) -> SkillDecl {
    SkillDecl {
        name: name.to_string(),
        title: format!("{name} (title)"),
        content: content.to_string(),
        preconditions: r#"["the repo is cloned"]"#.to_string(),
        steps: r#"[{"code_id": 1, "note": "list the issues"}]"#.to_string(),
        verification: r#"["the output parses as JSON"]"#.to_string(),
    }
}

/// The engine is 96 MB and not in the repo, so a machine without the model
/// skips the ranking test and still runs everything else (the discipline
/// `graph.rs` uses).
fn local_embedder() -> Option<std::sync::Arc<dyn mem_core::Embedder>> {
    let cfg_path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/krystallizer.kdl");
    let cfg = mem_core::KrystallizerConfig::load(&cfg_path).ok()?;
    let dir = cfg.embedding.resolved_model_dir().ok()?;
    if !dir.join("model.safetensors").exists() {
        eprintln!("skipping: no embedding model in {}", dir.display());
        return None;
    }
    let engine: std::sync::Arc<dyn mem_core::Embedder> =
        std::sync::Arc::new(mem_core::LocalEmbedder::load(&dir).expect("model loads"));
    Some(engine)
}

#[test]
fn declaring_twice_is_declaring_once_and_the_parts_merge() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    let first = graph.put_skill(1, &decl("github-issues", "Lists a repo's issues.")).unwrap();
    assert!(first.created, "the first declaration creates the node");

    // A second declaration with a new content and NO preconditions: the parts
    // that are written merge, and an absent part leaves the old one alone
    // (an empty string is absence, not "undeclare").
    let mut again = decl("github-issues", "Lists a repo's OPEN issues.");
    again.preconditions = String::new();
    let second = graph.put_skill(1, &again).unwrap();
    assert!(!second.created, "the name already exists: same skill");
    assert_eq!(second.node_id, first.node_id, "the id stays");

    let SkillRead::Found(skill) = graph.skill(1, first.node_id).unwrap() else {
        panic!("the skill reads back");
    };
    assert_eq!(skill.content, "Lists a repo's OPEN issues.");
    assert_eq!(
        skill.preconditions, r#"["the repo is cloned"]"#,
        "an absent part leaves the declared one alone"
    );
}

#[test]
fn the_four_parts_round_trip_and_the_kind_is_checked() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    let landed = graph.put_skill(1, &decl("github-issues", "Lists a repo's issues.")).unwrap();

    let SkillRead::Found(skill) = graph.skill(1, landed.node_id).unwrap() else {
        panic!("the skill reads back");
    };
    assert_eq!(skill.name, "github-issues");
    assert_eq!(skill.title, "github-issues (title)");
    assert_eq!(skill.preconditions, r#"["the repo is cloned"]"#);
    assert_eq!(skill.steps, r#"[{"code_id": 1, "note": "list the issues"}]"#);
    assert_eq!(skill.verification, r#"["the output parses as JSON"]"#);

    // Absence and a wrong kind are different answers: a caller that asked for
    // the wrong id needs to know which mistake it made.
    assert_eq!(graph.skill(1, 999).unwrap(), SkillRead::Absent);

    // A fact's endpoint is a node too, and its id lives in the same space.
    graph
        .ingest(
            1,
            &FactTuple::from_json(r#"["alice","person",{},"likes",{},"tea","drink",{}]"#).unwrap(),
        )
        .unwrap();
    let (alice, _) = graph.entity_by_name(1, "alice").unwrap().unwrap();
    match graph.skill(1, alice).unwrap() {
        SkillRead::NotASkill { kind } => assert_eq!(kind, "person"),
        other => panic!("alice is not a skill: {other:?}"),
    }
}

#[test]
fn a_skill_without_a_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    let refusal = graph.put_skill(1, &decl("  ", "nothing to call it by")).unwrap_err();
    assert!(
        refusal.to_string().contains("needs a name"),
        "the refusal names the missing thing: {refusal}"
    );
}

/// No engine: retrieval lands nothing, so the resolver falls back to the kind
/// face — and still answers only skills, in id order, cut at the limit.
#[test]
fn without_an_engine_the_resolver_scans_the_kind() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();

    for name in ["github-issues", "post-to-slack", "read-a-pdf"] {
        graph.put_skill(1, &decl(name, "does a thing")).unwrap();
    }
    // A node of another kind in the same scope: the resolver must not offer it.
    graph
        .ingest(
            1,
            &FactTuple::from_json(r#"["alice","person",{},"likes",{},"tea","drink",{}]"#).unwrap(),
        )
        .unwrap();

    let all = graph.resolve_skills(1, "anything at all", DEFAULT_LIMIT).unwrap();
    let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["github-issues", "post-to-slack", "read-a-pdf"]);
    assert!(
        all.iter().all(|c| c.score == 0.0),
        "a scan measures no distance, so it reports none"
    );

    let cut = graph.resolve_skills(1, "anything", 2).unwrap();
    assert_eq!(cut.len(), 2, "the limit is honoured");

    // The clamp is the core's, not the door's: a caller cannot ask for the
    // whole corpus by naming a huge limit.
    let clamped = graph.resolve_skills(1, "anything", 10_000).unwrap();
    assert!(clamped.len() <= MAX_LIMIT);
}

/// With the real model: a query lands on the skill that says the same thing,
/// and the candidate carries the text a planner reads.
#[test]
fn with_an_engine_the_query_lands_on_the_skill_that_says_it() {
    let Some(engine) = local_embedder() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap().with_embedder(engine);

    graph
        .put_skill(1, &decl("github-issues", "Lists the open issues of a GitHub repository."))
        .unwrap();
    graph
        .put_skill(1, &decl("read-a-pdf", "Extracts the text layer of a PDF document."))
        .unwrap();

    let hits = graph.resolve_skills(1, "open issues in a github repo", 2).unwrap();
    assert!(!hits.is_empty(), "the query lands on something");
    assert_eq!(hits[0].name, "github-issues");
    assert!(hits[0].score > 0.0, "a landed candidate carries its score");
    assert!(
        hits[0].content.contains("open issues"),
        "the candidate carries the text a planner reads: {}",
        hits[0].content
    );
}

/// The skill node rides the fact graph's weight machinery: its experience is
/// its incident facts, so a fact about a skill is an ordinary edge.
#[test]
fn experience_is_an_ordinary_fact_about_the_skill() {
    let dir = tempfile::tempdir().unwrap();
    let mut graph = GraphStore::open(dir.path()).unwrap();
    let landed = graph.put_skill(1, &decl("github-issues", "Lists a repo's issues.")).unwrap();

    graph
        .ingest(
            1,
            &FactTuple::from_json(
                r#"["github-issues","skill",{},"WORKED_ON",{"since":1761000000000},"acme_api","repository",{}]"#,
            )
            .unwrap(),
        )
        .unwrap();

    // The same node, not a second one: resolution found the declared skill.
    let (again, _) = graph.entity_by_name(1, "github-issues").unwrap().unwrap();
    assert_eq!(again, landed.node_id);
    let facts = graph.out_facts(1, landed.node_id).unwrap();
    assert_eq!(facts.len(), 1);
    assert_eq!(facts[0].kind, "WORKED_ON");

    // The props the declaration wrote survive the ingest that touched the node.
    let props = graph.entity_props(1, landed.node_id).unwrap().unwrap();
    assert!(props.contains_key(STEPS_FIELD));
    assert!(props.contains_key(PRECONDITIONS_FIELD));
    let row = graph.entity(1, landed.node_id).unwrap().unwrap();
    assert_eq!(row.kind.as_str(), Some(SKILL_KIND));
}