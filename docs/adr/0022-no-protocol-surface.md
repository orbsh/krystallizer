# 0022 — No protocol surface: no MCP, no A2A, no skill files

> **Languages:** [English](0022-no-protocol-surface.md) (primary) · [中文](0022-no-protocol-surface.zh-CN.md)

**Status:** Accepted (2026-10-28). A standing refusal; it adds no surface and no door.
**Date:** 2026-10-28

## Context

ADR-0001 settled what k10r's surfaces are: session control, memory, tasks — and it named one consumption stance (memory-primary) as a stance, not a fourth surface. What it did not settle is what k10r refuses to be: the industry shape of a memory/skill service in 2026 is an MCP server (tools exposed over JSON-RPC to any willing client), or an agent-to-agent endpoint that peers register against; skills are conventionally a directory of `SKILL.md` files that a host scans and injects.

Every one of those shapes is an answer to a problem this system does not have: interoperation between unfamiliar implementations. A protocol pays for itself when the peer is foreign — a different vendor, a different runtime, an operator who installed something else. And a skill directory pays for itself when the capability list is bounded, the ownership is one person's, and the boundaries are drawn by hand — a few dozen skills, one host, one maintainer.

The target form is neither. The consumers of k10r are homogeneous agent loops produced by this same stack, and the organization it serves is liquid: crews form against a goal and dissolve, capability boundaries drift with use, and permission is topology rather than a per-object grant. In that form the protocol's dynamic trust surface (anything may register, anything may be hot-plugged) is a liability, and a file's fixed boundary is a container that no longer matches the thing being contained.

## Decision

**RULING — k10r exposes the three surfaces of ADR-0001 and stops there. It implements no MCP server, no A2A-class endpoint, and no skill-file loader.**

1. **The doors are the interface.** `/channels`, `/memories`, `/skills`, `/tasks`, `/code`, `/graph` (SEAM.md §1) are what a consumer calls, over one transport (mem-server's HTTP; mem-core linked in-process on the same machine is an implementation detail, ADR-0007 as revised). A foreign client that wants to use k10r writes an adapter for those doors; k10r does not grow a second face to meet it.

2. **No MCP server.** A tool k10r does not serve cannot be advertised: the tool face is gravity's fixed, hardcoded list (gravity ADR-0004), and every entry there is one of k10r's doors. Wrapping those doors in JSON-RPC would add a channel per connection (identity, capability declaration, schema version, timeouts, retries, delivery semantics, idempotency) to serve peers that do not exist in this deployment, and a registry to say which tools are live — a dynamic surface over a substrate whose whole design is append-only, policy-free truth.

3. **No A2A-class endpoint.** k10r is not an agent and does not address one. A skill's execution goes through gravity's walk (`resolve_skills` → `invoke_skill` → a node mount), never through a peer discovered in a registry. Coordination between agents is state that k10r already holds: facts and task vertices (ADR-0021). An agent-to-agent channel here would duplicate that state into messages and put a second failure surface beside the graph.

4. **A skill is a node, not a file** (ADR-0015). `POST /skills/{scope}` is the declaration door; resolve answers candidates by intent; the step's code is a row (ADR-0012). No directory is scanned, no `SKILL.md` is injected, and no boundary is drawn by hand — the boundary is a subgraph that drifts with use. A file is one export of that subgraph at a point in time, which is why the project's own tooling may still render one without k10r reading it back.

5. **The refusal is one criterion applied three times.** The ecosystem axis measures the handshake cost between unfamiliar implementations; the extension axis measures cost and loss that grow with the number of entities. k10r's payoff on the first axis is zero in this deployment, and the pressure on the second is maximal — so both axes point the same way, and the answer is structural rather than a matter of taste.

## Considered and rejected

- **An MCP server in front of the doors.** It serves clients this stack does not have, and it makes the capability list a runtime registry — the one thing a substrate that never rewrites truth must not own.
- **An A2A endpoint so another framework's agent could ask k10r a question.** The same question is a door call; the same answer is a projection. What the protocol adds is a second name for each operation and a channel to keep alive.
- **A skill-directory loader (`SKILL.md` scan + frontmatter index).** It assumes a bounded count and human-drawn boundaries. Neither holds once the count grows past a few dozen and skills start merging and splitting under use; the loader would then be maintaining a snapshot that contradicts the graph it is drawn from.
- **A plugin/tool registry that accepts registration at runtime.** Registration is a write to the capability boundary performed by whoever registers — the opposite of a substrate whose writes are appends to facts under the caller's scope.

## Consequences

- **Callers adapt; the core does not.** Any future foreign consumer writes an adapter against SEAM.md's doors. This is the correct direction of work: the adapter knows its own dialect, and k10r keeps one vocabulary.
- **No dynamic trust surface.** What k10r can serve is fixed by its doors and its schema, not by what someone registered — the audit story is the append log, not a registry's configuration history.
- **The doors must be enough.** If a consumer needs something the three surfaces do not answer, the answer is a door or a projection change reviewed as an ADR — not a protocol bolted on the side.
- **The cost lands on distribution, deliberately:** a file directory is copied and a protocol is installed, while k10r's shape must be reached through its doors. That cost is paid once per deployment and is not paid again per agent — which is exactly the trade a liquid organization wants.

## Related records

ADR-0001 (the surfaces and the memory-primary stance), ADR-0007 (the consumption boundary as revised: HTTP is the single cross-process form), ADR-0012 (code rows), ADR-0015 (a skill is a node), ADR-0021 (tasks are vertices with a sheet), gravity ADR-0004 (the fixed tool list) and gravity ADR-0006 (coordination is parallelism, not messaging). The organizational form is argued in the wiki: `graph-memory.md` §液态化 and `liquefaction.md`.