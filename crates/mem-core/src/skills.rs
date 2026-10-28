//! The skill node (ADR-0015): the row `resolve_skills` lists and
//! `invoke_skill` walks.
//!
//! A skill is an entity node of kind `skill` in the fact graph (ns 36), in the
//! same scope as the facts — the resolver's question ("which of these fits the
//! task in front of me") is the graph's retrieval question, so the vector, the
//! landing distance and the weight system are all borrowed rather than rebuilt.
//!
//! The four parts, and where each lives (ADR-0015 §3):
//!
//! - `preconditions` / `steps` / `verification` are PROPS carrying JSON text.
//!   The store does not parse them: the producer owns the shape, exactly as
//!   `CodeRow::parameters` does (ADR-0012 §2). A prop the store parsed would be
//!   a second schema to migrate.
//! - `experience` is NOT a prop. It is the skill node's incident facts, where
//!   the weight system (ADR-0005) already counts a use and decays it.
//!
//! Identity is the node's `name`: declaring twice is declaring once (the props
//! merge, the id stays), which is the resolution rule `ingest` already follows.

use okm_core::obj_dynamic::DynamicValue;
use std::collections::BTreeMap;

use crate::graph::{GraphStore, IngestError, Term, CONTENT_FIELD, TITLE_FIELD};

/// The node kind a skill lands under. A kind is a vocabulary label, not an
/// allocation (okm ADR-0032), so nothing reserves it.
pub const SKILL_KIND: &str = "skill";

/// What must hold before the first step — JSON array of strings.
pub const PRECONDITIONS_FIELD: &str = "preconditions";
/// The steps, in execution order — JSON array of `{"code_id": n, "note": "…"}`.
pub const STEPS_FIELD: &str = "steps";
/// What to check after a step — JSON array of strings.
pub const VERIFICATION_FIELD: &str = "verification";

/// How often this skill was STARTED (`invoke_calls`) and how often it finished
/// (`invoke_ok`). Usage telemetry on the node, not the weight system's counters,
/// which ride EDGE rows and rank retrieval (ADR-0005/ADR-0018). A failed attempt
/// still counts as a call: "tried and broke" is the state a reader is looking for.
///
/// These three names are FIELDS on `EntityRow` (layout v5), not props — the
/// constants stay so a caller writing through `merge` keeps one spelling.
pub const CALLS_FIELD: &str = "invoke_calls";
pub const OK_FIELD: &str = "invoke_ok";
/// The caller's clock at the last count, so "hot" and "abandoned" differ.
pub const LAST_INVOKED_FIELD: &str = "last_invoked";

/// Which record of an invocation this is. Two events, because a caller records
/// the attempt BEFORE it knows the outcome: the store never has to infer one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TouchPhase {
    /// The call was dispatched, outcome unknown.
    Attempt,
    /// The call finished; the answer came back.
    Success,
}

impl TouchPhase {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "attempt" => Some(Self::Attempt),
            "success" => Some(Self::Success),
            _ => None,
        }
    }
}

/// The counters after one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SkillTouch {
    pub node_id: u64,
    pub calls: u64,
    pub ok: u64,
}

/// Recording an invocation: three outcomes, because the door reports them as
/// three (absent → 404, another kind → 400, counted → the counters).
#[derive(Clone, Debug, PartialEq)]
pub enum SkillTouchOutcome {
    Counted(SkillTouch),
    Absent,
    NotASkill { kind: String },
}

/// A resolver's answer without a query: `limit` is what the door applies when
/// the caller names none, and `MAX_LIMIT` is where a caller stops being
/// answered a list and starts being answered a dump.
pub const DEFAULT_LIMIT: usize = 5;
pub const MAX_LIMIT: usize = 20;

/// A skill as it is declared. Absent parts are empty strings (the store keeps
/// no opinion about JSON), and `name` is the identity.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SkillDecl {
    pub name: String,
    pub title: String,
    pub content: String,
    pub preconditions: String,
    pub steps: String,
    pub verification: String,
}

/// A skill as it is read back — the declaration plus the id it landed on.
#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub node_id: u64,
    pub name: String,
    pub title: String,
    pub content: String,
    pub preconditions: String,
    pub steps: String,
    pub verification: String,
}

impl Skill {
    fn from_row(node_id: u64, name: String, props: BTreeMap<String, DynamicValue>) -> Self {
        let text = |field: &str| match props.get(field) {
            Some(DynamicValue::Str(value)) => value.clone(),
            _ => String::new(),
        };
        Self {
            node_id,
            name,
            title: text(TITLE_FIELD),
            content: text(CONTENT_FIELD),
            preconditions: text(PRECONDITIONS_FIELD),
            steps: text(STEPS_FIELD),
            verification: text(VERIFICATION_FIELD),
        }
    }
}

/// One candidate a resolver hands back: enough for a model to decide, and
/// nothing more.
///
/// `steps` rides along because DECIDING needs it: the model has to see what a
/// skill would make it do before choosing it, and gravity's fixed list has no
/// "read one skill" call (ADR-0004 — a fifth tool name is a bigger ruling than
/// a field). `preconditions` / `verification` do NOT ride along: they are what
/// the walk reads once a skill is chosen, and the walk is gravity's business.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillCandidate {
    pub node_id: u64,
    pub name: String,
    pub title: String,
    pub content: String,
    /// The steps, in execution order — JSON text, as declared.
    pub steps: String,
    /// Retrieval score, `0.0` for a candidate that came from the kind-face
    /// fallback (a scan has no distance to report — never a fake one).
    pub score: f32,
}

/// The answer to "declare this skill": the id it landed on, and whether this
/// call is what created it.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillLanded {
    pub node_id: u64,
    pub created: bool,
}

/// Reading one skill: three outcomes, because the door reports them as three
/// (absent → 404, another kind → 400, found → the row). Folding "not a skill"
/// into absence would tell a caller its id is wrong when the id is fine.
#[derive(Clone, Debug, PartialEq)]
pub enum SkillRead {
    Found(Box<Skill>),
    Absent,
    NotASkill { kind: String },
}

impl GraphStore {
    /// Declare a skill (idempotent on `name`): the props merge, and `created`
    /// says whether this call is what brought the node into being.
    pub fn put_skill(&mut self, scope: u64, decl: &SkillDecl) -> Result<SkillLanded, IngestError> {
        if decl.name.trim().is_empty() {
            return Err(IngestError::Refused(
                "a skill needs a name: it is the identity the resolver returns".to_string(),
            ));
        }
        let mut props: BTreeMap<String, DynamicValue> = BTreeMap::new();
        for (field, value) in [
            (TITLE_FIELD, &decl.title),
            (CONTENT_FIELD, &decl.content),
            (PRECONDITIONS_FIELD, &decl.preconditions),
            (STEPS_FIELD, &decl.steps),
            (VERIFICATION_FIELD, &decl.verification),
        ] {
            // Empty means ABSENT: the resolution path merges props, so writing
            // an empty string would overwrite a part a previous declaration
            // filled — "undeclare this part" is not a call anyone makes.
            if !value.trim().is_empty() {
                props.insert(field.to_string(), DynamicValue::Str(value.clone()));
            }
        }
        let term = Term {
            value: decl.name.clone(),
            type_label: SKILL_KIND.to_string(),
            props,
        };
        // A node that already exists keeps its kind: resolution merges props
        // and never re-labels (an entity that has been a `person` for a month
        // must not become a `skill` because a declaration said so).
        let (node_id, created) = self.put_entity(scope, &term)?;
        Ok(SkillLanded { node_id, created })
    }

    /// One skill by id. The kind is checked, because a skill id and a fact
    /// node's id live in the same space.
    pub fn skill(&mut self, scope: u64, node_id: u64) -> Result<SkillRead, IngestError> {
        let Some(row) = self.entity(scope, node_id)? else {
            return Ok(SkillRead::Absent);
        };
        let kind = row.kind.as_str().unwrap_or_default().to_string();
        if kind != SKILL_KIND {
            return Ok(SkillRead::NotASkill { kind });
        }
        let props = self.entity_props(scope, node_id)?.unwrap_or_default();
        Ok(SkillRead::Found(Box::new(Skill::from_row(
            node_id, row.name, props,
        ))))
    }

    /// Count one record of an invocation (ADR-0018): `Attempt` when the caller
    /// dispatches, `Success` when the answer came back. A failed call therefore
    /// leaves `calls > ok`, which is the whole point of recording the attempt
    /// first — the store never has to guess an outcome.
    ///
    /// The counters are DECLARED FIELDS on the node row (ADR-0018 amendment,
    /// 2026-10-27): a number every call bumps is the opposite of "application
    /// data that arrives one at a time", and reading it back out of the dynamic
    /// segment costs a map lookup for a value the touch was already rewriting
    /// the row for. `merge` routes a name that matches a declared field to the
    /// row and leaves the props alone, so one touch is still ONE write.
    pub fn touch_skill(
        &mut self,
        scope: u64,
        node_id: u64,
        phase: TouchPhase,
        now: u64,
    ) -> Result<SkillTouchOutcome, IngestError> {
        let Some(row) = self.entity(scope, node_id)? else {
            return Ok(SkillTouchOutcome::Absent);
        };
        let kind = row.kind.as_str().unwrap_or_default().to_string();
        if kind != SKILL_KIND {
            return Ok(SkillTouchOutcome::NotASkill { kind });
        }
        let calls = row.invoke_calls + u32::from(phase == TouchPhase::Attempt);
        let ok = row.invoke_ok + u32::from(phase == TouchPhase::Success);
        let mut write: BTreeMap<String, DynamicValue> = BTreeMap::new();
        write.insert(
            CALLS_FIELD.to_string(),
            DynamicValue::UInt(u64::from(calls)),
        );
        write.insert(OK_FIELD.to_string(), DynamicValue::UInt(u64::from(ok)));
        write.insert(LAST_INVOKED_FIELD.to_string(), DynamicValue::UInt(now));
        self.merge_props(scope, node_id, &write)?;
        Ok(SkillTouchOutcome::Counted(SkillTouch {
            node_id,
            calls: u64::from(calls),
            ok: u64::from(ok),
        }))
    }

    /// The selectable list (ADR-0015 §4): retrieval over the skill kind, and —
    /// when retrieval lands nothing — the kind-face scan, in id order.
    ///
    /// Nothing is counted: a candidate nobody chose is not a read (ADR-0005).
    /// That is why this takes no clock while `recall` does.
    pub fn resolve_skills(
        &mut self,
        scope: u64,
        query: &str,
        limit: usize,
    ) -> Result<Vec<SkillCandidate>, IngestError> {
        let limit = limit.clamp(1, MAX_LIMIT);
        // Over-fetch: the graph holds facts and literals too, so a query that
        // lands k nodes lands fewer than k skills. 4× is enough without asking
        // the index for the whole corpus.
        let seeds = self.retrieve(scope, query, limit * 4)?;
        let mut out: Vec<SkillCandidate> = Vec::new();
        for seed in seeds {
            if seed.kind != SKILL_KIND {
                continue;
            }
            let props = self.entity_props(scope, seed.node_id)?.unwrap_or_default();
            let skill = Skill::from_row(seed.node_id, seed.name, props);
            out.push(SkillCandidate {
                node_id: skill.node_id,
                name: skill.name,
                title: skill.title,
                content: skill.content,
                steps: skill.steps,
                score: seed.score,
            });
            if out.len() == limit {
                return Ok(out);
            }
        }
        if !out.is_empty() {
            return Ok(out);
        }
        // No engine, or nothing inside the landing distance: the corpus of
        // skills is small by construction, so the answer is a scan rather than
        // "there are no skills" (ADR-0015 §4).
        for (node_id, row) in self.scope_entities(scope)? {
            if row.kind.as_str() != Some(SKILL_KIND) {
                continue;
            }
            let props = self.entity_props(scope, node_id)?.unwrap_or_default();
            let skill = Skill::from_row(node_id, row.name, props);
            out.push(SkillCandidate {
                node_id: skill.node_id,
                name: skill.name,
                title: skill.title,
                content: skill.content,
                steps: skill.steps,
                // A scan measures no distance, so it reports none.
                score: 0.0,
            });
            if out.len() == limit {
                break;
            }
        }
        Ok(out)
    }
}
