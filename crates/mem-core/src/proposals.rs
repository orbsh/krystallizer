//! Skill PROPOSALS (ADR-0016): what real work suggests, kept where the model
//! cannot see it.
//!
//! The read side of the skill loop is landed (ADR-0015) and the write side used
//! to be a human with curl. This is the write side's landing place: gravity — the
//! only component that sees a turn's actual composition — reports "this ordered
//! run of code rows happened" and k10r counts it. Nothing here promotes itself.
//!
//! Three decisions worth the words:
//!
//! - **A proposal is an entity node, not a new store.** Same graph, same scope as
//!   the facts and the skills, kind `skill_proposal`. The resolver keeps only the
//!   `skill` kind, so a proposal is invisible to the model until a human promotes
//!   it — which is the point: ADR-0003 §5 says a recurring composition is
//!   PROPOSED, never self-parented.
//! - **The composition key is the CALLER's**, because the store does not parse
//!   `steps` (ADR-0015 §3). gravity walked those code rows, so gravity says what
//!   the run was; k10r only has to spell it as a name and count it.
//! - **`count` is a prop, not a weight.** The weight system's counters ride EDGE
//!   rows (ADR-0005), and a proposal is a node. Counting here is application
//!   state — how many times did this run happen — not retrieval ranking, and it
//!   is read back by a human door, not by the resolver.
//!
//! Promotion is a DECLARATION, not a rename: the human's `POST /skills` is the
//! act, with the human's name and title, and the proposal records what it became.
//! Re-labelling a node in place would need a write path the graph does not have,
//! and it would lose the record of the run that suggested it.

use okm_core::obj_dynamic::DynamicValue;
use std::collections::BTreeMap;

use crate::graph::{GraphStore, IngestError, Term};
use crate::skills::{SkillDecl, STEPS_FIELD};

/// The node kind a proposal lands under — a vocabulary label beside `skill`,
/// never a version of it.
pub const PROPOSAL_KIND: &str = "skill_proposal";

/// How many times this composition has been walked (a prop, per the note above).
pub const COUNT_FIELD: &str = "count";
/// The caller's clock at the last walk, ms.
pub const LAST_SEEN_FIELD: &str = "last_seen";
/// The skill this proposal became, once a human declared one.
pub const PROMOTED_TO_FIELD: &str = "promoted_to";

/// What gravity reports: which composition ran (`key`), its steps as a JSON
/// document (the same shape a skill's `steps` takes, so promotion copies rather
/// than translates), and when.
#[derive(Clone, Debug, PartialEq)]
pub struct ProposalDecl {
    /// The composition, spelled by the walker: the code ids in the order they
    /// ran, e.g. `"4+7"`. Empty is refused — an identity nobody can name is not
    /// an identity.
    pub key: String,
    pub steps: String,
    pub ts_ms: u64,
}

/// A proposal as it is read back.
#[derive(Clone, Debug, PartialEq)]
pub struct Proposal {
    pub node_id: u64,
    pub key: String,
    pub steps: String,
    pub count: u64,
    pub last_seen: u64,
    /// Empty until a human declares the skill it suggested.
    pub promoted_to: String,
}

/// The answer to "this composition just ran": the row it landed on, how many
/// times it has now been seen, and whether this call created it.
#[derive(Clone, Debug, PartialEq)]
pub struct ProposalLanded {
    pub node_id: u64,
    pub count: u64,
    pub created: bool,
}

/// Reading one proposal for promotion: the same three outcomes `skill` reports,
/// for the same reason (absent is not "not a proposal").
#[derive(Clone, Debug, PartialEq)]
pub enum ProposalRead {
    Found(Box<Proposal>),
    Absent,
    NotAProposal { kind: String },
}

/// The answer to "no, a HUMAN is declaring this one": the skill that landed.
#[derive(Clone, Debug, PartialEq)]
pub struct Promoted {
    pub skill_node_id: u64,
    pub name: String,
    pub created: bool,
}

impl GraphStore {
    /// Count one walk of one composition (idempotent on the key: a repeat is the
    /// same proposal with a higher count, never a second row).
    pub fn propose(
        &mut self,
        scope: u64,
        decl: &ProposalDecl,
    ) -> Result<ProposalLanded, IngestError> {
        let key = decl.key.trim();
        if key.is_empty() {
            return Err(IngestError::Refused(
                "a proposal needs a key: the composition is its identity".to_string(),
            ));
        }
        let name = format!("proposal:{key}");
        // Read-modify-write on the count, inside the store's single writer. The
        // alternative — a delta table like the weight system's — buys contention
        // behaviour this row will never see: one proposal per distinct
        // composition, written once per turn that ran it.
        let count = match self.entity_by_name(scope, &name)? {
            Some((node_id, _)) => {
                let props = self.entity_props(scope, node_id)?.unwrap_or_default();
                count_of(&props) + 1
            }
            None => 1,
        };

        let mut props: BTreeMap<String, DynamicValue> = BTreeMap::new();
        // An empty `steps` stays absent for the usual reason: props merge, so
        // writing "" would erase what a previous walk reported (ADR-0015).
        if !decl.steps.trim().is_empty() {
            props.insert(
                STEPS_FIELD.to_string(),
                DynamicValue::Str(decl.steps.clone()),
            );
        }
        props.insert(COUNT_FIELD.to_string(), DynamicValue::UInt(count));
        props.insert(LAST_SEEN_FIELD.to_string(), DynamicValue::UInt(decl.ts_ms));
        let term = Term {
            value: name,
            type_label: PROPOSAL_KIND.to_string(),
            props,
        };
        let (node_id, created) = self.put_entity(scope, &term)?;
        Ok(ProposalLanded {
            node_id,
            count,
            created,
        })
    }

    /// One proposal by id, kind checked (a skill id and a proposal id share a
    /// node space).
    pub fn proposal(&mut self, scope: u64, node_id: u64) -> Result<ProposalRead, IngestError> {
        let Some(row) = self.entity(scope, node_id)? else {
            return Ok(ProposalRead::Absent);
        };
        let kind = row.kind.as_str().unwrap_or_default().to_string();
        if kind != PROPOSAL_KIND {
            return Ok(ProposalRead::NotAProposal { kind });
        }
        let props = self.entity_props(scope, node_id)?.unwrap_or_default();
        Ok(ProposalRead::Found(Box::new(Proposal {
            node_id,
            key: strip_prefix(&row.name),
            steps: text_of(&props, STEPS_FIELD),
            count: count_of(&props),
            last_seen: uint_of(&props, LAST_SEEN_FIELD),
            promoted_to: text_of(&props, PROMOTED_TO_FIELD),
        })))
    }

    /// The review queue: proposals seen at least `min_count` times and not yet
    /// promoted, most-walked first.
    ///
    /// A scan rather than a declared face, deliberately: the queue is read by a
    /// human, it is small by construction (one row per distinct composition),
    /// and nothing here is on the model's path. A weight face would rank by
    /// retrieval, which is the wrong question — the queue's question is "did
    /// this run REPEAT".
    pub fn proposals(&mut self, scope: u64, min_count: u64) -> Result<Vec<Proposal>, IngestError> {
        let mut out: Vec<Proposal> = Vec::new();
        for (node_id, row) in self.scope_entities(scope)? {
            if row.kind.as_str() != Some(PROPOSAL_KIND) {
                continue;
            }
            let props = self.entity_props(scope, node_id)?.unwrap_or_default();
            let proposal = Proposal {
                node_id,
                key: strip_prefix(&row.name),
                steps: text_of(&props, STEPS_FIELD),
                count: count_of(&props),
                last_seen: uint_of(&props, LAST_SEEN_FIELD),
                promoted_to: text_of(&props, PROMOTED_TO_FIELD),
            };
            if proposal.promoted_to.is_empty() && proposal.count >= min_count.max(1) {
                out.push(proposal);
            }
        }
        out.sort_by(|a, b| b.count.cmp(&a.count).then(a.node_id.cmp(&b.node_id)));
        Ok(out)
    }

    /// Promote: the human declares a skill, and the proposal records what it
    /// became. The steps are COPIED from the proposal, so the skill starts with
    /// exactly what ran; the name, title and content are the human's.
    pub fn promote(
        &mut self,
        scope: u64,
        node_id: u64,
        name: &str,
        title: &str,
        content: &str,
    ) -> Result<Promoted, IngestError> {
        let proposal = match self.proposal(scope, node_id)? {
            ProposalRead::Found(proposal) => *proposal,
            ProposalRead::Absent => {
                return Err(IngestError::Refused(format!(
                    "no proposal has node id {node_id}"
                )))
            }
            ProposalRead::NotAProposal { kind } => {
                return Err(IngestError::Refused(format!(
                    "node {node_id} is a `{kind}`, not a proposal"
                )))
            }
        };
        if !proposal.promoted_to.is_empty() {
            return Err(IngestError::Refused(format!(
                "this proposal is already a skill: `{}`",
                proposal.promoted_to
            )));
        }
        let landed = self.put_skill(
            scope,
            &SkillDecl {
                name: name.to_string(),
                title: title.to_string(),
                content: content.to_string(),
                preconditions: String::new(),
                steps: proposal.steps.clone(),
                verification: String::new(),
            },
        )?;

        // Record the outcome on the proposal. Same name and kind, so resolution
        // merges this prop onto the row it already is — `count` and `steps`
        // survive because an absent prop is never a deletion.
        let mut props: BTreeMap<String, DynamicValue> = BTreeMap::new();
        props.insert(
            PROMOTED_TO_FIELD.to_string(),
            DynamicValue::Str(name.to_string()),
        );
        let term = Term {
            value: format!("proposal:{}", proposal.key),
            type_label: PROPOSAL_KIND.to_string(),
            props,
        };
        self.put_entity(scope, &term)?;

        Ok(Promoted {
            skill_node_id: landed.node_id,
            name: name.to_string(),
            created: landed.created,
        })
    }
}

/// The proposal's key as the walker spelled it (the name carries a prefix so a
/// human reading the graph knows what the row is).
fn strip_prefix(name: &str) -> String {
    name.strip_prefix("proposal:").unwrap_or(name).to_string()
}

fn text_of(props: &BTreeMap<String, DynamicValue>, field: &str) -> String {
    match props.get(field) {
        Some(DynamicValue::Str(value)) => value.clone(),
        _ => String::new(),
    }
}

fn uint_of(props: &BTreeMap<String, DynamicValue>, field: &str) -> u64 {
    match props.get(field) {
        Some(DynamicValue::UInt(value)) => *value,
        Some(DynamicValue::Int(value)) => (*value).max(0) as u64,
        _ => 0,
    }
}

fn count_of(props: &BTreeMap<String, DynamicValue>) -> u64 {
    uint_of(props, COUNT_FIELD)
}
