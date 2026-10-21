# ADR-0013: A backup is a Parquet export over the same door

Date: 2026-10-20
Status: RULED & LANDED 2026-10-20 — the document tables. **Update 2026-10-21**: the
door is addressed by TABLE NAME rather than ns, a catalog file carries name↔ns, the
fact table exports as okm's edge table, and reduces travel as derived tables (§2, §4,
§5). The reasons in §3 are unchanged; the reasons in the old §4 were wrong and are
corrected there.

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

## 2. Decision

**`GET /export/{table}` — one table, one file, `application/vnd.apache.parquet`.**
**`GET /export` — the catalog: `(table, ns)`, one row per table.**

- The ADDRESS is the table's NAME, not its ns. The ns is the store's own
  addressing (ADR-0010 §1's vocabulary): it identifies a table *inside* this store
  and means nothing to whoever opens the file. A backup is read outside, so it is
  named outside. A name is declared where the ns is declared — okm `#[ok_name]` →
  `Document::TABLE_NAME`, beside `#[ok_ns]` — so the two cannot drift, and a rename
  moves no stored byte.
- The catalog is the name↔ns direction as its own file. Pure analysis does not need
  it (the exported columns are already labeled); an import does, and so does the
  question "what is ns 41 called". It is generated at export time from the same
  declarations, so it is not a second table to keep in step.
- `Content-Disposition` names the file `<table>.parquet`, so a directory of exports
  says what it holds without opening anything.
- The schema is okm's, not k10r's: typed columns, `Vocab` fields exported as their
  LABEL text (and NULL where an id has no name), fixed-width declared fields as
  their width, `Vector` fields as their elements. k10r decides which table and
  where the file goes — nothing else.
- okm's `export_parquet` writes to a PATH, so the door writes into a temp directory
  and returns the bytes. The answer is a SNAPSHOT at request time: writes that land
  after the response was built are not in that file.
- A name this service does not hold is a 404 that points at the catalog. An empty
  table is a 200 with a valid, empty Parquet file — "this table is empty" is
  information a backup should carry.
- The row's OPEN half travels as a `variant` column: the declared fields stay typed
  columns, and the dynamic segment (an edge's props, an entity's attributes) is one
  column beside them. Without it the file holds the typed half of every row and
  loses the rest silently — which is what the first cut of this door did.

**Granularity: per ns, one file per table.** The alternative — one archive per
backup — buys a single file at the cost of a container format plus a second parser
in the restore path, and it hides which table failed. Table-per-file also means a
partial restore is a table, not an extraction exercise.

## 3. Why the format is not k10r's

A k10r-written dump would be a second description of the same rows: it would have
to be kept in step with the declaration, it would carry k10r's field names into
whatever reads the backup, and it would fail the one test a backup has — being
readable by a tool that is not the program that wrote it. Parquet gives that for
free (polars, duckdb, pyarrow all open the file), and the vocabulary-label rule
means the file is legible without the store's numbering tables.

## 4. The fact table is a table, and the first answer here was wrong

The first cut answered `GET /export/37` with **501**: edges are not `Document`
rows — they live in the graph's own entry shape (`EdgeKey` + `EdgeBody` frames,
`GraphEncode`) and okm's bridge reads `Document` collections — so the argument was
that a k10r-invented edge dump would be a second format beside the real one.

The refusal was right about the second format and wrong about where the gap was.
A backup that omits the facts is not a partial backup; it is a backup of the nodes
of a graph whose whole content is the edges. And the shape was never in doubt: an
edge's endpoints ARE (table, primary key) pairs, and its declared fields are already
a declaration. So the export surface went on okm's `Graph` (okm ADR-0034 §3, §4) and
k10r's fact table exports like every other table:

- columns: `src_table, src_pkey, dst_table, dst_pkey, predicate`, then the edge
  declaration's fields, then the `variant` column of the assertion's props;
- the endpoint keys stay RAW bytes — one edge table spans node collections and
  cannot decode another table's key without that table's declaration;
- `name_of` resolves an ns through the catalog, and an ns it does not know is NULL,
  never a guessed name.

## 5. Reduces are their own derived table

`log__reduces`, `code__reduces`: `(table, reduce, group, value)`, one row per
reduce entry. A reduce is not a row — it lives in its own slot family and holds an
accumulator — but a restore cannot do without it: **a restored table whose watermark
is missing hands out ids it already used.** A restored log that appends over an
existing `seq` is silent corruption, so the watermark travels with the table that
depends on it.

## 6. Consequences

- Inspection and restore-by-hand need no k10r: the file carries labels, not ids.
- The structured log can be archived before the trim drops its rows — the
  one table where the export is also the only long-term record.
- **Restore is NOT implemented.** okm has `import_parquet`, and wiring it is a
  separate door with its own rulings: which ns, replace or merge, and what to do
  when a `Vocab` label in the file is not in the destination vocabulary (okm's
  import refuses rather than guessing — the refusal is the design, not an
  oversight).
- Replaying a reduce table is a restore obligation, not an export one: import the
  rows, then push the watermark to the file's maximum, then allow a write.
- The `parquet` feature is always on in this build. The door is a first-class
  surface, not an optional add-on, so the dependency (arrow + parquet) is paid once
  at build time instead of becoming a second, differently-built binary.
