# ADR-0007: Consumption Boundary — One Cross-Process Form (HTTP), No Python Layer

**Status**: Accepted
**Date**: 2026-09-14
**Chinese**: [0007-consumption-boundary.zh-CN.md](0007-consumption-boundary.zh-CN.md)
**Supersedes**: ADR-0004's layer-ownership decision (the Python adapter layer)

## Context

ADR-0004 placed the extraction pipeline's LLM orchestration in "the
Python layer (skillforge side, later the mem adapter package)". That
reflected how skillforge happened to be implemented, not a property of
this project. Krystallizer's direction is **Python-free**: it is a
Rust memory core consumed by other projects, and it does not need a
language binding to be one (FFI's withdrawal is item 3 below).

The consumer of k10r is settled (ADR-0006): gravity, the stateless
agent. Two boundaries are already fixed elsewhere and carry over:

- **LLM calls belong to gravity** (model identity is bound to the side
  that initiates inference); the single exception is krystallizer's
  internal embedding (the embedding space is bound to data correctness,
  so the memory system holds it).
- **gravity — not k10r — is the aura-world citizen** (Milestone B
  Phase 6: gravity is hosted as a booth type). k10r stands outside
  aura entirely, in the same class as the AI endpoint: an external
  dependency whose contract is URL reachability.

## Decision

**Krystallizer is an independent network service — Fjall storage,
self-held embedding, HTTP API — pure Rust, no Python layer. It is not
an aura citizen: it registers no booth, is not spawned by aura, and
occupies no ns.**

1. **HTTP (mem-server) is the ONLY cross-process form.** mem-server
   graduates from "deferred shell" to the single remote surface, with
   the channel-log endpoints gravity needs (append / fetch_full /
   append_checkpoint as of ADR-0008's minimal slice).
2. **In-process mem-core is a local same-machine implementation
   detail, not an access mode.** Gravity Phase 0 links mem-core
   directly and calls `ChannelStore` in-process; that is one
   deployment choice on one machine, invisible to the boundary. The
   MemorySurface seam (in-process / HTTP implementations) is gravity's
   concern, not a krystallizer access mode.
3. **FFI is WITHDRAWN** (2026-10-21; `mem-ffi`, PyO3, is deleted). It was
   kept on two claims — "it has existing bindings" and "it remains the Phase 0
   binding smoke test" — and neither survived inspection: nothing in any repo
   consumed it, the crate carried no test of its own, and gravity's Phase 0
   links `mem-core` directly as a Rust dependency. Two reasons it could not have
   been repaired by adding tests: the binding attaches no embedding engine, so
   the most valuable read (`retrieve` / `recall`) answers EMPTY on that channel —
   a second shape of the same core that silently degrades is worse than no
   second shape; and a per-language binding multiplies by host language (Python
   today, whatever tomorrow), while HTTP does not. It also contradicted rule 1
   above rather than complementing it: putting the core inside another process
   IS a third form.

The earlier "three access modes" framing is withdrawn: there are not
three ways to reach k10r across a process boundary — there is exactly
one (HTTP), plus local-linking and bindings that are not boundary
shapes at all.

**k10r's API has two classes, and they are not interchangeable.** The
**core API** is what a consumer's operation runs on — the session projection
and appends, the memory answer (`recall`), the exact identity entrance, the
reported counter, the code store. Its SHAPES are contract: a change is this
ADR's class of decision, and a consumer's turn path may only use these doors.
The **analysis API** is read-only inspection for audit, visualization and
debugging — the raw graph, the landing in isolation, one hop, the unrouted
core READ primitives. It is a second-class surface: its shape may change as
the analysis needs it, it is never on a model's tool list, and no consumer's
turn loop may depend on it. The line is drawn where structure is allowed:
internal structure (node id, edge id, community id) may appear INSIDE a core
result as a handle, while MAINTAINING that structure is never a consumer's
chore — a door that asks a consumer to list, pull, fix up or write back k10r's
own indexing is neither class, it is a design error (the consolidation doors
retired on 2026-10-20 were exactly that). `docs/SEAM.md` keeps the two lists
apart, one table each.

**The two classes sit under two path prefixes**, so the class is visible in the
URL rather than in a table: `/memories/{scope}/…` is the core memory face (the
write, the answer, the identity entrance, the reported counter) and
`/graph/{scope}/…` is the analysis face (the whole graph, the landing alone, one
hop) — the session and code doors keep `/channels` and `/code`. Nothing is
served at the old `/facts` prefix: a renamed door is a broken door, and an alias
would let a consumer's turn keep depending on a shape the analysis face is free
to change.

**LLM orchestration belongs to gravity.** Extraction-pipeline LLM
decisions (stage classification, out-of-domain fusion decisions,
summarize when policy needs one) are gravity's work — k10r hands over
deterministic primitives and cheap half-steps (candidate sets), the
model-bound decisions happen on the side that owns the model identity.

**Embedding is internal to krystallizer, and it is not a network call
either.** The model is built in and runs in-process (ADR-0003): the
vector space is bound to data correctness, so the memory system — not
any caller — owns embedding consistency, and callers never supply
vectors. Write and query paths compute embeddings in-process.

**Secrets never enter the config file.** Configuration is one KDL
document (`krystallizer.kdl`, one top-level node per subsystem), read
from beside the binary unless `--config` names another; every secret is
referenced by the environment variable name that holds it. A path value
may carry `$HOME` — the machine's home is deployment data, not a
property of the software — and an unset `HOME` is a boot failure rather
than a directory literally named `$HOME`.

## Consequences

- ADR-0004's ownership claim ("extraction lives in the Python layer")
  is superseded; its remaining content — the deterministic
  `ingest(fact)` tuple surface, the fusion split (core = cheap half,
  caller = expensive half) — carries over with the caller now being
  gravity.
- **The aura-actor access mode is withdrawn (2026-10-10), including
  its 2026-10-09 bgi revision.** k10r is never a standalone process
  spawned by aura via a `language: "induction"` booth type, and k10r never
  runs as an in-aura actor at all. gravity supports booth/cli carrier
  modes; a k10r process hosted by aura has no reason to exist — the
  aura side reaches memory through gravity, which reaches k10r over
  HTTP. Two superseded shapes are on record: the original wasm form
  (OKM schema compiled into wasm, storage moved to aura via a
  NestStorage frame up-call) died on its own hole — a wasm sandbox
  holds no filesystem AND no outbound network, while the embedding
  call was the one network surface k10r owned (ADR-0003: the embedding
  space is bound to the data — moving it out is a correctness
  violation, not a preference; ADR-0034's withdrawal of wasi-http
  closed the only planned wasm egress path). The bgi form fixed that
  hole but kept the wrong premise: k10r as an aura-hosted process.
- **aura PLAN Phase 6.6 (Storage Booth / NestStorage executor) is
  withdrawn with it** — no aura-hosted executor was ever needed for
  k10r's storage. k10r always owns its Fjall directory (ADR-0002)
  regardless of who calls it; "one directory on disk" stands without a
  carriage split.
- mem-server is a deliverable, not a deferred shell: the channel-log
  endpoints first (append / fetch_full / append_checkpoint), then the code
  store, grown as consumers need; the flat-memory door landed and was then
  withdrawn with the flat table (2026-10-19).
- **The crate's outbound network surface stayed empty; the SHELL's is exactly
  one call, and it is the deployment's choice.** The embedding model is built in
  (ADR-0003), so where this ADR originally conceded exactly one call there was
  none: no LLM client, no transport framework, no embedding API. Ruled
  2026-10-21: `mode "remote"` in the `embedding` node lets a deployment send
  that ONE call to an OpenAI-shaped embeddings service. The boundary that
  matters is unchanged, and it is now drawn by CRATE rather than by sentence:
  `mem-core` still owns no client (the seam is a trait), and the process that
  already speaks HTTP to consumers is the one that speaks HTTP to a model. No
  LLM client, no model-bound decision and no consumer traffic rides that
  surface — it carries one text and one vector.
- A private config-parsing crate (knus-based, env-merge included) is
  planned once the third consumer need (KDL-shaped CLI) is real; for
  now knus alone covers decode, env reads stay one `std::env::var`
  away.