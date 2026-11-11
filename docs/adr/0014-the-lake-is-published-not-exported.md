# ADR-0014: The service publishes itself into a lake, and the manual export doors are withdrawn

Date: 2026-11-11
Status: RULED & LANDED 2026-11-11. Supersedes ADR-0013 (which stays as the record of the
bundle it replaced).

## 1. Context

ADR-0013 made the backup a REQUEST: `GET /export` built a `tar.zst` of every table beside its
Delta log, `GET /export/{table}` handed out one table, and `POST /import` put a bundle back.
Everything a backup needs was there except the two properties that decided it in practice.

The first is that a request has to be made. A backup that depends on somebody remembering is
a backup that exists on the days somebody remembered; the interesting failure is not a broken
export but an export nobody ran for two months.

The second is that a bundle is a FILE. Its consumer unpacks it, finds a directory, and from
there every reader is on its own — the format is right, but the thing being kept is a copy on
one disk, and moving it (to a bucket, to a warehouse, to a second machine) is again somebody's
job.

okm's lake line answers both: a table's exported directory is a Delta table (okm ADR-0058), the
log is the manifest, and the writer takes any key space — a directory or a bucket (okm
ADR-0061). A service that can write one Parquet file and one commit can BE the lake.

One table is not published under its own name: the edge set is published as one table per endpoint pair (`facts__entities__entities`, …), because a SQL/PGQ declaration can only reference one vertex table per edge table (ADR-0027).

## 2. Decision: a `lake` node, a publisher, and no export doors

**2.1 The configuration.** One optional top-level node in `krystallizer.kdl`:

```kdl
lake {
    target "s3://krystallizer/k10r"     // or a directory
    interval-secs 300
    s3 {
        endpoint "http://127.0.0.1:9000"
        key-env "K10R_S3_KEY"           // the NAME of the variable, never the key
        secret-env "K10R_S3_SECRET"
    }
}
```

`target` is ONE url: `s3://bucket/prefix` is a bucket, anything else is a directory, and the
writer adapts — the difference is a store implementation, not a code path in k10r. The `s3`
block exists only for a bucket and carries an endpoint (absent = AWS) and the names of the
environment variables holding the key pair. Secrets never enter the file; that rule is older
than this ADR and this node is no exception.

**Absence of the node is the switch.** There is no `enabled false`: a service either has a lake
or it does not, and "configured but disabled" is a state that lies.

**2.2 The publisher.** A task runs beside the doors — one pass when they open, then every
`interval-secs`. One pass copies every table out (the declared tables plus the derived reduce
tables), and publishes each as one Delta version whose `add` list is this snapshot's file and
whose `remove` list is the file it replaced. The lake therefore holds CURRENT STATE: one live
file per table, one version per interval, and the previous version's file deleted once the
commit lands.

The two phases are separate on purpose. A store's lock is taken only to copy a table into
memory and dropped immediately; the publishes then run on a blocking thread, one table at a
time. A slow bucket must not become a slow service.

**2.3 The doors are withdrawn.** `GET /export`, `GET /export/{table}` and `POST /import` are
removed, with the bundle format and the `bundle` module. ADR-0013 is superseded: its reasoning
about the format (declaration-generated Parquet, Delta metadata, no second storage) survives
verbatim in this decision — what is withdrawn is the transport, because a filesystem the
service writes to is not something a caller has to ask for.

**2.4 Failure is a log line, except at boot.** A lake that cannot be opened — a bad url, a
store that refuses, a named credential variable that is unset — is a START failure. A publish
that fails at runtime logs and is retried at the next interval: the store has not changed, the
next snapshot carries the same rows, and the lake is one interval behind rather than never
right. Serving stops for neither.

**2.5 What the schema says.** A document table's Delta schema marks its key columns
(`okm.key`), so a reader can fold the change table back to current state (okm ADR-0058). The
graph's edge batch and the two reduce tables carry no mark: their identity is not this
service's to state — an edge's key is the triple the edge set is keyed by, and a reduce row is
an accumulator, not a document.

## 3. Consequences

- **There is no restore path.** A restore now means "read the lake back", which is a different
  job: it would land the lake's rows through the same write contracts a live request uses, and
  it has to decide what to do with a store that is not empty. Left undone on purpose — the
  doors this ADR removes were the only restore, and a half-restore is worse than none.
- **The lake keeps history.** One version per interval, unbounded. Trimming it is `okm-lake
  vacuum`'s job against the same target, and it is an operator's decision how much history a
  mirror should keep.
- **The dependency grows in okm, not here.** The S3 client lives in `okm-lake`, behind its
  `store` module; k10r passes a url, a key pair and bytes.
- **A publish is a full snapshot.** For a service of this size that is the honest shape (the
  tables are small and the cost is a read each); when it stops being true, the answer is okm's
  change-log path, not a smarter snapshot.