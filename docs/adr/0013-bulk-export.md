# ADR-0013: A backup is one bundle of Parquet files with warehouse metadata

Date: 2026-10-20
Status: RULED & LANDED 2026-10-20 (per-table Parquet). **Re-ruled 2026-10-22**:
the backup is ONE `tar.zst` holding every table as a directory of Parquet files
beside Iceberg v2 metadata, and restore is a door. §2's per-table granularity and
the old §4's rejection of "one archive per backup" are superseded; §7 records why
the metadata is Iceberg's and not Delta Lake's.

## 1. Context

ADR-0007 decided that a bulk read is still a read over the same HTTP boundary: no
admin CLI, no second storage, no dump format invented beside the data. A backup is
exactly such a read — "everything in this table" — and the question it leaves open
is the FORMAT, not the transport.

Two answers are possible. Invent one (a JSON or CSV dump written by k10r), or use
the bridge okm already has: okm ADR-0007's eager Arrow export turns a collection
into `RecordBatch`es from the row DECLARATION and `parquet_io` writes them to a
file. The second is the only one that cannot drift: the columns are generated from
the same declaration that generates the key encoding and the payload frames, so a
schema change is one struct edit, and a rename is impossible without both moving.

## 2. Decision: one bundle, many tables, warehouse metadata

**`GET /export` — ONE file, `application/zstd`, named
`krystallizer-backup-<UTC>.tar.zst`.** Unpacked it is a directory:

```
krystallizer/
  catalog.parquet                    # (table, ns), one row per table
  <table>/data/<table>.parquet       # the table's rows
  <table>/metadata/v1.metadata.json  # Iceberg v2 table metadata
  <table>/metadata/<uuid>-m0.avro    # manifest (one entry per data file)
  <table>/metadata/snap-<id>-0-<u>.avro # manifest list
  <table>/metadata/version-hint.text # the version, so a DIRECTORY is readable
```

**`POST /import` — the same file, back.** One request restores every table.

- One file to move, many Parquet files to query. The earlier per-table export made
  the operator fetch N files and remember their order; that is the cost this
  re-ruling refuses — a backup is one artifact.
- The tables are exported under the same lock each write takes, one table at a
  time, into a temp directory, and the archive is built only when all of them
  succeeded: a failure is a 500 and no half bundle is served. So the bundle is a
  snapshot, and — unlike N separate files — every table in it is the same moment.
- The transport is `tar.zst`, not zip: the file is handled from a shell
  (`tar -xf`, `tar -tvf`). Compression and container live in `mem-core::bundle`
  (tar + zstd crates), never in okm: okm's metadata writer takes a DIRECTORY and
  data files and knows nothing about archives. Which means a container or codec
  change reaches one file, and a schema change reaches the other.
- The ADDRESS inside the bundle is the table's NAME, not its ns. The ns is the
  store's own addressing (ADR-0010 §1's vocabulary): it identifies a table *inside*
  this store and means nothing to whoever opens the file. A name is declared where
  the ns is declared — okm `#[ok_name]` → `Document::TABLE_NAME`, beside `#[ok_ns]`
  — so the two cannot drift, and a rename moves no stored byte.
- The catalog is the name↔ns direction as its own table at the root: pure analysis
  does not need it (the exported columns are already labeled), an import does, and
  so does the question "what is ns 41 called". It is generated in the same pass from
  the same declarations, so it is not a second table to keep in step.
- The schema is okm's, not k10r's: typed columns, `Vocab` fields exported as their
  LABEL text (NULL where an id has no name), fixed-width declared fields as their
  width, `Vector` fields as their opaque frame bytes. k10r decides which tables, the
  directory layout and the archive — nothing else.
- **`GET /export/{table}`** stays, as inspection: one table, one Parquet file, no
  metadata. It is not the backup's form. Reads are cheap to offer; the restore half
  stays a single door, so there is exactly one way to write a store's worth of rows
  from files.
- An empty table still exports a valid, empty Parquet file — "this table is empty"
  is information a backup carries — and an unknown name on the inspection door is a
  404 that points at the bundle.
- The row's OPEN half travels as a `variant` column: declared fields stay typed
  columns, and the dynamic segment (an edge's props, an entity's attributes) is one
  column beside them. Without it the file holds the typed half of every row and
  loses the rest silently — which is what the first cut of this door did.

**Granularity: per table inside one bundle.** One wide Parquet per backup is
impossible — a Parquet file carries ONE schema. The variant that pretends otherwise
(every table's columns unioned, plus a `table` column) throws away what made okm's
export trustworthy — the file IS the declaration — and turns a restore into a
re-splitting exercise.

## 3. Why the format is not k10r's

A k10r-written dump would be a second description of the same rows: it would have
to be kept in step with the declaration, carry k10r's field names into whatever
reads the backup, and fail the one test a backup has — being readable by a tool
that is not the program that wrote it. Parquet gives that for free (polars, duckdb,
pyarrow all open the file), and the vocabulary-label rule means the file is legible
without the store's numbering tables.

## 4. The fact table is a table, and the first answer here was wrong

The first cut answered `GET /export/37` with **501**: edges are not `Document` rows
— they live in the graph's own entry shape (`EdgeKey` + `EdgeBody` frames,
`GraphEncode`) and okm's bridge reads `Document` collections — so the argument was
that a k10r-invented edge dump would be a second format beside the real one.

The refusal was right about the second format and wrong about where the gap was. A
backup that omits the facts is not a partial backup; it is a backup of the nodes of
a graph whose whole content is the edges. And the shape was never in doubt: an
edge's endpoints ARE (table, primary key) pairs, and its declared fields are already
a declaration. So the export surface went on okm's `Graph` (okm ADR-0034 §3, §4) and
its IMPORT mirror went there too (`Graph::from_record_batch_table`):

- columns: `src_table, src_pkey, dst_table, dst_pkey, predicate`, then the edge
  declaration's fields, then the `variant` column of the assertion's props;
- the endpoint keys stay RAW bytes — one edge table spans node collections and
  cannot decode another table's key without that table's declaration — so the
  import resolves a NAME back to an ns and hands the raw key through;
- `name_of` resolves an ns through the catalog, and an ns it does not know is NULL,
  never a guessed name; the import refuses an unknown name BY ROW, never inventing
  a collection;
- the edge id rides the key column, so a restore keeps the ids the backup carries
  instead of renumbering.

## 5. Reduces are their own derived table

`log__reduces`, `code__reduces`: `(table, reduce, group, value)`, one row per reduce
entry. A reduce is not a row — it lives in its own slot family and holds an
accumulator — but a restore cannot do without it: **a restored table whose watermark
is missing hands out ids it already used.** A restored log that appends over an
existing `seq` is silent corruption, so the watermark travels with the table that
depends on it, and the import keeps the HIGHER of the two accumulators: a restore
never lowers a watermark, because a lower one re-issues ids live rows still carry.

## 6. Restore: merge by key, and the issuers follow the rows

`POST /import` writes through the SAME contracts the live paths use (`put`,
`link_into`, `put_fields_into`, the reduce entry), in the dependency order the
catalog declares — nodes and people before the edges that name them, a watermark
after the rows it counts. The operator never sorts files.

- **Merge, never wipe.** Same key overwrites, a new key inserts; the door never
  deletes. A door that can clear a store is a door a typo can destroy it with, and
  merge is idempotent: re-running a half-done restore completes it. "Start from
  nothing" is a deployment act (a fresh `storage.data-dir`), not an endpoint.
- **The issuers are recomputed from the store afterwards** (`GraphStore::reseed`,
  `LogStore::reseed`, `CodeStore::reseed`): next node id, next edge id, the
  community sequence cache, the log's `seq`, the code id. A restored store that
  skipped this would hand out an id a restored row already holds — the failure §5's
  watermark rule prevents, one level up.
- A label a `Vocab` column carries but the destination vocabulary lacks is
  INTERNED there (okm's import does this, and the ids in the restored rows are the
  destination's own numbering). The earlier text here predicted a refusal: that was
  wrong about okm, and wrong about what a restore wants — a snapshot carries names,
  not codes.
- A failed import is a PARTIAL import: rows already written stay. Merge semantics is
  what makes that safe (fix, re-run); there is no cross-table transaction to pretend
  otherwise.
- Refusals are the caller's bug: a body that is not a `tar.zst`, an archive without
  the bundle directory, a traversal-shaped entry, a table member the catalog names
  but the archive lacks, a column set that is not the table's declaration, an
  endpoint collection this store does not have.
- The community sequence (`ns 38`) is in the bundle as its own table. It is an
  ISSUER, not data: without it a restored graph re-issues community ids the restored
  nodes already carry.

## 7. Why Iceberg's metadata and not Delta Lake's

Both formats are a log of Parquet files, and for a snapshot-shaped export Delta's
log is the smaller thing: three JSON lines (`protocol`, `metaData`, `add`) and no
Avro, where Iceberg needs a metadata JSON plus a manifest and a manifest list, and
requires column FIELD IDs inside the Parquet files.

The ruling is still Iceberg, on the criterion this repo spends on anything durable —
what the file has to keep being readable as, for years:

- The expensive half of Iceberg is the manifests' Avro encoding with the spec's
  field ids, and it is written and reader-verified here (DuckDB `iceberg_scan`,
  pyiceberg). That cost is sunk, so Delta's cheaper first cut no longer buys
  anything.
- Iceberg's reader base is widening, its REST catalog is becoming the default, and
  the major warehouses read it natively; Delta's OSS momentum is thinner and its
  new features land engine-first. A backup format is a long-lived promise, and this
  repo should not rewrite the promise in a year.
- Incremental exports (not built) fit Iceberg's own shape: a new export is snapshot
  N+1 pointing at a new manifest list, with the earlier snapshot still readable.
  Delta would be equally capable, so this is not a discriminator — it is why the
  snapshot is the unit we model.
- What Delta would genuinely win — a log a human can `cat` — is not worth the swap:
  the manifest is readable by the tools the file is handed to, and nobody debugs a
  five-year-old backup by reading its log.

Where the two are equal: multi-table access. Neither gives a warehouse you can point
at without a catalog service, which is why the bundle carries `catalog.parquet` and
every table is a self-describing directory (DuckDB reads one table by directory,
pyiceberg by metadata path, and a consumer wanting a catalog registers the tables in
its own).

## 8. Consequences

- Inspection and restore-by-hand need no k10r: the file carries labels, not ids, and
  the metadata is a published spec's.
- The structured log can be archived before the trim drops its rows — the one table
  where the export is also the only long-term record.
- `parquet` is always on in this build, and the archive dependencies (tar, zstd) are
  mem-core's alone: okm gains no compression and no container code.
- A per-table backup taken before this re-ruling is not readable by `POST /import`.
  Pre-production: no such backup exists.