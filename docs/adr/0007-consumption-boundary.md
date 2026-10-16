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
Rust memory core consumed by other projects; mem-ffi is one binding,
not an architecture layer.

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
3. **FFI stays** (mem-ffi, PyO3): it has existing bindings and remains
   the Phase 0 binding smoke test. It is a consumption channel, not a
   layer of krystallizer; other bindings (or none) are equally
   legitimate.

The earlier "three access modes" framing is withdrawn: there are not
three ways to reach k10r across a process boundary — there is exactly
one (HTTP), plus local-linking and bindings that are not boundary
shapes at all.

**LLM orchestration belongs to gravity.** Extraction-pipeline LLM
decisions (stage classification, out-of-domain fusion decisions,
summarize when policy needs one) are gravity's work — k10r hands over
deterministic primitives and cheap half-steps (candidate sets), the
model-bound decisions happen on the side that owns the model identity.

**Embedding is internal to krystallizer** — the only network call in
the core (already conceded by ADR-0003). The embedding model is locked
with the data: vectors from different models do not share a space, so
the memory system, not any caller, owns embedding consistency. Write
and query paths compute embeddings in-process.

**Secrets never enter the config file.** Configuration is one KDL
document (`config/krystallizer.kdl`, one top-level node per
subsystem); every secret is referenced by the environment variable
name that holds it.

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
  call is the one network surface k10r owns (ADR-0003: the embedding
  space is bound to the data — moving it out is a correctness
  violation, not a preference; ADR-0034's withdrawal of wasi-http
  closed the only planned wasm egress path). The bgi form fixed that
  hole but kept the wrong premise: k10r as an aura-hosted process.
- **aura PLAN Phase 6.6 (Storage Booth / NestStorage executor) is
  withdrawn with it** — no aura-hosted executor was ever needed for
  k10r's storage. k10r always owns its Fjall directory (ADR-0002)
  regardless of who calls it; "one directory on disk" stands without a
  carriage split.
- mem-server is a deliverable, not a deferred shell: three endpoints
  first (append / fetch_full / append_checkpoint), grown as consumers
  need.
- The core's own network surface remains exactly one outbound call:
  the embedding API. No LLM client, no transport framework.
- A private config-parsing crate (knus-based, env-merge included) is
  planned once the third consumer need (KDL-shaped CLI) is real; for
  now knus alone covers decode, env reads stay one `std::env::var`
  away.

## Update (2026-10-14): the core's outbound network surface is empty

ADR-0003's embedding question is decided as a **built-in local model**
(ADR-0003 Update 2026-10-14), so the one network call this ADR conceded
("the only network call in the core", "exactly one outbound call: the
embedding API") no longer exists. Nothing else moves: embedding still
belongs to krystallizer — the vector space is bound to data correctness
and the model stays locked with the data — and callers still never supply
vectors. The consequence is a strictly narrower boundary, not a new
freedom: no LLM client, no transport framework, and now no embedding API
either.
