# ADR-0026: A restore goes into an empty namespace

Date: 2026-11-11
Status: RULED & LANDED 2026-11-11

## 1. Context

The lake is published (ADR-0014), so the rows leave. Nothing was bringing them back. A restore has exactly two uses here: a whole service rebuilt from its own lake, and part of a corpus moved into a namespace an application has just created — the second one is how an analytics side takes a copy without a second declaration per destination.

Both uses share one property that decides everything else: the rows must not meet rows that are already there.

## 2. Decision: refuse first, then land

**A restore only writes into namespaces that hold nothing.** The check runs over every target namespace before a single row is written; one non-empty target aborts the whole restore with the offender named (`facts in ns 37`). There is no merge, no overwrite, no skip. A half-restored table is a table whose rows come from two moments, and no reader of it can tell which row is from when.

**Two shapes, and only two.**

- **No mapping** — every table the lake holds lands in the namespace its declaration names. A full restore.
- **A mapping of lake table → ns** — only the named tables land, in the namespaces the caller chose. That is partial import, and it is also migration: the destination names the place, okm never moves a table on its own.

Anything else (merge into an existing corpus, restore "the rest") is out of scope by decision, not by omission.

**The emptiness question is asked of the key header**, so faces, index entries and reduce rows count as occupancy. Asking a document scan instead would let a restore land on top of an index whose entries point at rows that no longer exist.

## 3. How a table lands

Through `Collection::rebased(ns)` (okm ADR-0062), so the same declaration writes under the new namespace and the vocabulary is interned into the DESTINATION's numbering: ids are local, labels are the identity, so a mapped restore of the same lake into two namespaces yields the same labels and different ids. `Graph::rebased(ns)` for edges, with a resolver that answers each endpoint table's destination ns — an edge's endpoints are `[ns 2B]` references, so an edge moved alone would point at nothing.

The reduce tables (a log's `seq` watermark, a code sheet's id watermark) are not addressable on their own: they ride their parent table's ns and are skipped when the parent is. A restore without them hands out ids the restored rows already carry.

## 4. Where the door is

The binary, not HTTP:

    krystallizer --config <file> --restore <target> [--map <table>=<ns>]...

The store has one writer, so a restore is an operator action on a stopped service. It lands the lake, prints what landed where, and exits without opening the door — a service that restored and then started serving would be two writers of the same accepted state.

## 5. Consequences

Restore is the inverse of publish and nothing else: no surface was added, no protocol, no endpoint. The lake directory or bucket is the only input, which also means a restore needs no running source service — the sending side can be gone.

Accepted costs: a restore is all-or-nothing per invocation (a table that fails means redoing the whole plan, because the namespaces are no longer empty and a partial run must be cleaned by hand); and the emptiness check is a scan per target namespace, bounded by how much a namespace holds — paid once, before any write.