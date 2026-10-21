# ADR-0013: A backup is a Parquet export over the same door

Date: 2026-10-20
Status: RULED & LANDED 2026-10-20 — the document tables. The fact table is owed
(ns 37, see §4).

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

**`GET /export/{ns}` — one table, one file, `application/vnd.apache.parquet`.**

- The ns addresses the table, exactly as it does everywhere else (ADR-0010 §1's ns
  vocabulary). `Content-Disposition` names the file `ns<N>.parquet`, so a directory
  of exports says what it holds without opening anything.
- The schema is okm's, not k10r's: typed columns, `Vocab` fields exported as their
  LABEL text (and NULL where an id has no name), fixed-width declared fields as
  their width, `Vector` fields as their elements. k10r decides which table and
  where the file goes — nothing else.
- okm's `export_parquet` writes to a PATH, so the door writes into a temp directory
  and returns the bytes. The answer is a SNAPSHOT at request time: writes that land
  after the response was built are not in that file.
- An ns this service does not hold is a 404. An empty table is a 200 with a valid,
  empty Parquet file — "this table is empty" is information a backup should carry.

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

## 4. The fact table is a 501, and why that is the right answer for now

Edges are not `Document` rows. An edge lives in the graph's own entry shape
(`EdgeKey` + `EdgeBody` frames, `GraphEncode`) and okm's bridge reads `Document`
collections only. So `GET /export/37` answers **501 with the reason**, not an empty
file and not a k10r-invented dump: a "backup" that cannot be read back into a store
is not a backup, and a second format beside the one real export is worse than a
named gap.

The fix belongs in okm: an export surface on `Graph` (the same `RecordBatch` shape,
edge columns from the body + the payload's declared fields). Until it exists, a
backup of k10r is a backup of everything except the facts — which is a sentence
worth having in the documentation rather than in a surprise.

## 5. Consequences

- Inspection and restore-by-hand need no k10r: the file carries labels, not ids.
- The structured log (ns 41) can be archived before the trim drops its rows — the
  one table where the export is also the only long-term record.
- **Restore is NOT implemented.** okm has `import_parquet`, and wiring it is a
  separate door with its own rulings: which ns, replace or merge, and what to do
  when a `Vocab` label in the file is not in the destination vocabulary (okm's
  import refuses rather than guessing — the refusal is the design, not an
  oversight).
- The `parquet` feature is always on in this build. The door is a first-class
  surface, not an optional add-on, so the dependency (arrow + parquet) is paid once
  at build time instead of becoming a second, differently-built binary.
