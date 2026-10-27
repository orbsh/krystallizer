//! Phase 2.6's compression policy engine (ADR-0008 §4).
//!
//! The policy is k10r's own and it is PURE: three HEURISTIC signals, no
//! LLM call, no timer, no scheduling machinery. It evaluates at the
//! projection read because that is where all three inputs already are —
//! the increment's own timestamps (the max-gap split), the assembled
//! view's size (the budget), and the member's read cursor (the cache
//! clock: the prompt cache is idle exactly while that member's
//! projection has not been read).
//!
//! The verdict rides the projection as a CONTROL FIELD beside the
//! records, never as a marker written into a log row. The caller runs
//! the extra branch turn out of band and hands the summary back through
//! the checkpoint door; k10r derives the coverage point from the span it
//! proposed, so the caller never computes or states one. There is no
//! agent-side compression interface — the caller cannot request this.
//!
//! Every number is a config parameter (`compression { … }`): tuning is
//! deferred until feedback labels exist, since fitting policy to
//! unlabelled traces fits noise.

use crate::channel::{SessionView, Span};

/// Policy parameters — `config/krystallizer.kdl`'s `compression` node.
#[derive(Clone, Debug, PartialEq)]
pub struct Params {
    /// The caller's model window, in BYTES. k10r holds no tokenizer for
    /// the consumer's model (the embedding model's is the wrong one to
    /// count with), so the budget is measured in the assembled text's
    /// bytes — a unit the caller can state from its own deployment.
    pub context_bytes: usize,
    /// Fire at this share of `context_bytes` — a MID band, not 90%:
    /// tiered pricing and summary quality both degrade late.
    pub budget_percent: u8,
    /// The provider's prompt-cache lifetime. Idle past it means the next
    /// turn pays a cold prefix for the same span.
    pub cache_ttl_ms: u64,
    /// Below this the view is too small for a summary to buy anything;
    /// the density of early turns is all signal.
    pub floor_bytes: usize,
}

impl Params {
    /// The budget in bytes the trigger compares against.
    pub fn budget_bytes(&self) -> usize {
        self.context_bytes
            .saturating_mul(self.budget_percent as usize)
            / 100
    }
}

/// The numbers `config/krystallizer.kdl` ships, mirrored for callers that
/// do not load a config (tests, bring-up). A serving deployment reads the
/// file — `config::tests::the_repo_config_decodes` asserts the two agree,
/// so this cannot drift from the file silently.
impl Default for Params {
    fn default() -> Self {
        Self {
            context_bytes: 131_072,
            budget_percent: 50,
            cache_ttl_ms: 300_000,
            floor_bytes: 2_048,
        }
    }
}

/// Which signal fired. Recorded in the decision log — with label-free
/// traces the only way to tune later is to know what fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    Budget,
    CacheClock,
    Both,
}

impl Trigger {
    /// The name the diagnostic log carries: a log row is read by a person or a
    /// query, and a word beats a code nobody remembers.
    pub fn name(self) -> &'static str {
        match self {
            Trigger::Budget => "budget",
            Trigger::CacheClock => "cache_clock",
            Trigger::Both => "both",
        }
    }

    /// The stored code (`DecisionRow::trigger`, retired with that table — the
    /// log row carries the name instead).
    pub fn code(self) -> u8 {
        match self {
            Trigger::Budget => 0,
            Trigger::CacheClock => 1,
            Trigger::Both => 2,
        }
    }
}

/// What the engine proposes: the span to compress, why, and the whole
/// parameter snapshot the decision log keeps.
#[derive(Clone, Debug, PartialEq)]
pub struct Verdict {
    /// The coverage point the summary should end at — the last log record of
    /// the span (ADR-0008 §4: k10r derives this, the caller never states
    /// one). A POINTER: `ts_ms` plus the same-millisecond tiebreaker, so the
    /// checkpoint can name the record rather than the millisecond.
    pub checkpoint_ts: u64,
    pub checkpoint_sub: u16,
    pub trigger: Trigger,
    pub assembled_bytes: usize,
    pub context_bytes: usize,
    pub budget_percent: u8,
    pub cache_ttl_ms: u64,
    /// Milliseconds since the read cursor; `None` = no clock rode the
    /// read, so the cache clock could not speak.
    pub idle_ms: Option<u64>,
    /// The largest inter-message gap inside the span; `None` = the span
    /// held no internal gap, so its end was the split.
    pub max_gap_ms: Option<u64>,
}

/// Evaluate the policy for one projection read.
///
/// `assembled_bytes` is the size of the context the caller is about to
/// build (head + summary + increment, in the representation it asked
/// for) — the engine cannot compute it, because the head is rendered
/// per `?format=` above this layer.
///
/// `idle_ms` is `now − read_at` when the caller supplied a clock and
/// the member has a cursor; `None` means the cache clock stays silent
/// (no clock, no signal — the same discipline as recall's "no clock, no
/// count").
///
/// `None` = do not compress: too small, no signal fired, or the split
/// would not advance past the current coverage point.
pub fn evaluate(
    view: &SessionView,
    assembled_bytes: usize,
    idle_ms: Option<u64>,
    params: &Params,
) -> Option<Verdict> {
    if view.increment.is_empty() || assembled_bytes < params.floor_bytes {
        return None;
    }
    let budget_hit = assembled_bytes >= params.budget_bytes();
    let cache_hit = matches!(idle_ms, Some(idle) if idle >= params.cache_ttl_ms);
    let trigger = match (budget_hit, cache_hit) {
        (false, false) => return None,
        (true, false) => Trigger::Budget,
        (false, true) => Trigger::CacheClock,
        (true, true) => Trigger::Both,
    };
    let (checkpoint, max_gap_ms) = split(view)?;
    Some(Verdict {
        checkpoint_ts: checkpoint.0,
        checkpoint_sub: checkpoint.1,
        trigger,
        assembled_bytes,
        context_bytes: params.context_bytes,
        budget_percent: params.budget_percent,
        cache_ttl_ms: params.cache_ttl_ms,
        idle_ms,
        max_gap_ms,
    })
}

/// The split point: the span ends at the LAST record before the largest
/// inter-message gap (a zero-cost topic-change proxy), and at the span's
/// own end when it holds no internal gap.
///
/// The silence that fired the cache-clock trigger is deliberately not
/// reachable here: it sits AFTER the increment's last record (now minus
/// that record's ts), never between two records, so "the gap IS the
/// silence that fired the trigger" cannot select itself. Ties go to the
/// earlier gap, so one corpus always answers identically.
fn split(view: &SessionView) -> Option<((u64, u16), Option<u64>)> {
    let last = view.increment.last()?;
    let mut best: Option<(u64, (u64, u16))> = None;
    for pair in view.increment.windows(2) {
        let gap = pair[1].0.ts_ms.saturating_sub(pair[0].0.ts_ms);
        if best.is_none_or(|(seen, _)| gap > seen) {
            best = Some((gap, (pair[0].0.ts_ms, pair[0].0.sub)));
        }
    }
    let (checkpoint, max_gap_ms) = match best {
        Some((gap, pointer)) => (pointer, Some(gap)),
        None => ((last.0.ts_ms, last.0.sub), None),
    };
    if checkpoint <= (view.coverage_ts, view.coverage_sub) {
        return None;
    }
    Some((checkpoint, max_gap_ms))
}

/// The tail prompt the verdict carries (ADR-0008 §4: the caller appends
/// it to the span's records and runs that as a branch turn beside the
/// user's turn — the answer IS the summary).
///
/// `docs/PROMPTS.md` §3 is this text's documented home; this copy is the
/// one the door serves, so the two are kept word-identical.
pub const TAIL_PROMPT: &str = "Two products from the conversation span above, as ONE JSON object and nothing else.\n\n\
{\"summary\": \"...\", \"cut\": <integer>}\n\n\
1. \"summary\": a summary an agent can continue from alone. Keep: decisions and their reasons, facts, \
names, numbers, identifiers and code verbatim, open questions, and what the user is trying to achieve. \
Drop: politeness, repetition, and reasoning that led nowhere. Do not answer the last message, do not \
ask questions, do not invent anything that is not in the span.\n\
2. \"cut\": the number of the LAST numbered record the summary should cover. Choose the boundary where \
the conversation breaks — the end of a request and its answer, not the middle of one. Use the last \
number when the whole span belongs together. The numbers are already in the span; do not copy them into \
the summary.";

/// One TAIL OPERATION: an instruction appended after the span and run as a
/// branch beside the user's turn. The caller appends `prompt` to the records it
/// read, and the model's answer is that operation's product (ADR-0008 §4).
///
/// The list is a TABLE because these operations differ in what they PRODUCE, not
/// in how they are driven: adding one is a new entry here plus its landing arm in
/// the caller — no shape change on the caller's side, no config knob. Today there
/// is one; the review pass (reading from the suggested cut) is why the table
/// exists rather than a single field.
pub struct TailOp {
    /// The name the caller dispatches on. It is also the key the projection
    /// carries, so a caller that does not know an operation SKIPS it instead of
    /// failing — deployment skew: the store may know more than the guest does.
    pub name: &'static str,
    /// The prompt appended after the span.
    pub prompt: &'static str,
    /// Which of the two cursors this operation reads from (ADR-0020).
    pub stage: Stage,
}

/// The two stages of the tail (ADR-0020). Compression is MANDATORY — without it
/// the context stops fitting — so it has its own cursor (the coverage point) and
/// its own trigger (the budget). Everything else is a review: not needed for the
/// answer, so it reads from the OTHER cursor, the cut the last compression said it
/// would have chosen, and it can be skipped without breaking anything.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    /// Covers the span this read asks to compress: `(coverage, checkpoint]`.
    Mandatory,
    /// Reads from the suggested cut — or from coverage when no compression has
    /// named one — to the newest record. A model's boundary, not a byte budget's.
    Optional,
}

/// The compression pass: summarize the span, and name the cut it would have
/// chosen over the mechanical one.
pub const COMPRESS_OP: TailOp = TailOp {
    name: "compress",
    prompt: TAIL_PROMPT,
    stage: Stage::Mandatory,
};

/// The review pass's prompt. `docs/PROMPTS.md` §2b is its documented home; this
/// copy is the one the door serves, so the two are kept word-identical.
pub const REVIEW_PROMPT: &str = "Review the conversation span above and answer ONE JSON object and nothing else.\n\n\
{\"facts\": []}\n\n\
\"facts\" holds 8-slot tuples [head value, head type, head props, relation name, relation props, \
tail value, tail type, tail props] — the shape commit_memory takes. Save what a later turn would \
otherwise have to rediscover: durable facts about the user, their systems, their decisions and \
their preferences, with names and identifiers spelled exactly as they appear. Do not restate the \
summary, do not record what mattered only to this exchange, and do not invent anything that is \
not in the span. An empty array is the right answer when nothing stands out — and is cheaper \
than a bad fact.";

/// The review pass: what the conversation should have written down and did not.
///
/// Its product is a fact array in the shape `commit_memory` already takes, so it
/// lands through the door that tool uses — no second schema, and no new gate. It
/// deliberately does NOT create skills: what the model let pass belongs in the
/// review queue a human promotes (ADR-0016 §6), and an operation that wrote
/// skills directly would make that queue pointless.
pub const REVIEW_OP: TailOp = TailOp {
    name: "review",
    prompt: REVIEW_PROMPT,
    stage: Stage::Optional,
};

/// Every operation the store may ask for: compression (mandatory, its own cursor)
/// and the review (optional, reads from the cut compression suggested).
pub const TAIL_OPS: &[TailOp] = &[COMPRESS_OP, REVIEW_OP];

/// The range one operation reads on this read (ADR-0020).
///
/// Mandatory: exactly the span the verdict asks to compress. Optional: from the
/// suggested cut — the boundary a model saw the conversation break at instead of
/// where a byte budget did — to the newest record, which is what an analysis pass
/// wants to look at. Both are stated as `((ts, sub), (ts, sub))`: a position in
/// the log is a RECORD, so a millisecond alone names several.
///
/// `None` = nothing to read, and the caller is not asked for that operation. The
/// fallback is deliberate: a compression that named no better cut leaves the
/// optional operations reading the span coverage already covers, rather than
/// skipping them — silence from the compressor is not a range.
pub fn tail_span(op: &TailOp, view: &SessionView, verdict: &Verdict) -> Option<Span> {
    let span = match op.stage {
        Stage::Mandatory => (
            (view.coverage_ts, view.coverage_sub),
            (verdict.checkpoint_ts, verdict.checkpoint_sub),
        ),
        Stage::Optional => {
            let from = if (view.suggested_ts, view.suggested_sub) == (0, 0) {
                (view.coverage_ts, view.coverage_sub)
            } else {
                (view.suggested_ts, view.suggested_sub)
            };
            let (last, _) = view.increment.last()?;
            (from, (last.ts_ms, last.sub))
        }
    };
    (span.0 < span.1).then_some(span)
}
