# 0025 — A channel is a row, and membership reads both ways

> **Languages:** [English](0025-a-channel-is-a-row.md) (primary) · [中文](0025-a-channel-is-a-row.zh-CN.md)

**Status:** Accepted (2026-11-04). All four sections are implemented: `apps` ns 43,
`channels` ns 45 with `app_id`, `members` ns 42 keyed by the member id alone, `memberships`
ns 46 with `by_member`, and the four doors.
**Date:** 2026-11-04

## Context

A channel existed only as a NUMBER inside other tables' key prefixes. `MessageKey` is
`[channel_id][turn][ts][sub]`, `CheckpointKey` and `CursorKey` likewise; `members` is keyed
`[channel_id][member_id]`. Nothing anywhere said what a channel IS, and the two questions an
application actually asks had no answer:

- **"Which channels does this person have?"** — `members` starts with the CHANNEL, so this
  was a full read of the table, in the wrong dimension.
- **"Which of my own channels is this?"** — there was nothing to filter by: no owner, no
  application, no label. An application could only have inferred it from the log.

The application side made the gap concrete. gravity needs one channel per user: prism
resolves a device to a user (ADR-0017 of prism: `device_user(DeviceId) -> Option<UserId>`),
gravity then must find the single channel that belongs to it. That is a storage question —
"this person's channels, the ones my application owns, expect exactly one" — and the store
had no shape for it.

## Decision

**1. A channel is a row** (`channels`, ns 45): `[channel_id]` → `{ app_id, scene, created_ms }`.

- `app_id` answers *which application owns this channel* — the OIDC `client_id`, as the `Uuid7`
  it is, pointing at the `apps` table (ns 43: `[app_id]` → `{ name, created_ms }`). Written by
  the deployment and never relabelled, because it is a filter key: the "exactly one" invariant
  is checked against it. Free text cannot serve — `"gravity"` in two spellings would be two
  applications.
- `scene` answers *what the channel is for*, in the writer's own words (`private`,
  `group:ops`). Free text on purpose. A closed enum would make every new scene a migration.

The two are separate fields because they have different owners and different lifetimes: a key
the machine filters on must not change when a human renames a label.

**2. Identity is global; membership is the relation** (`members` ns 42, `memberships` ns 46).

`members` is keyed `[member_id]` alone and carries `{ display_name, role }` — ONE row per
person, whatever channels they are in. Slicing it by channel was the first cut, and it made
"who is this uuid" unanswerable without naming a channel first.

`memberships` is keyed `[channel_id][member_id]` → `{ member_id, nickname, since_ms }`, with
`by_member` declared over the member column. The channel leads because the channel is the
container: deleting one purges its relationships as a single key range, while "which channels
is this person in" is the index's job. `nickname` is the per-channel name, empty meaning "use
the identity's `display_name`" — the same "empty means absent" rule the checkpoint summary
uses, since `DocumentEncode` has no `Option<T>`. `join()` writes the identity row and the
relationship row in one batch and commits once.

**3. Reads are filtered at the storage line.** `channels_of_app(app_id)` (equality on the app
id), `channels_of(member_id)` (the `by_member` index, fetch-back per hit), `members(channel_id)`
(prefix over the relationship), and the doors `GET /channels?app=`,
`GET /members/{id}/channels?app=`. The relationship carries no app of its own: the filter is
applied to the channel row it names.

**4. The member id IS the user id, and it is a `Uuid7`.** prism mints it; k10r stores the
same value in `MemberKey`, `MembershipKey`, `CursorKey`, the message log's sender and the
graph's `member-{uuid}` node name. No numeric surrogate, no hash of the uuid: an id in a log
that cannot be turned back into the identity it names is a dead end for every later question
("who wrote this"), and `Uuid7` is a fixed-width 128-bit value, so the key layout stays
fixed-width — only wider.

## Why not a junction

A many-to-many relationship is an ordinary table, and okm has ordinary tables: a key made of
the two identities plus whatever columns the relationship owns. `memberships` is exactly that,
so no junction carrier exists here — okm's `Junction` was retired (ADR-0051 there) after the
same conclusion was verified against its own test suite.

What the plain table buys, over a carrier that stores an empty pair:

- **the relationship can carry data** — `since_ms`, and the per-channel `nickname`;
- **the reverse direction is a declared index**, maintained by `put`/`purge` in the same batch
  and rebuildable by `rebuild_faces`, instead of a second hand-written entry;
- **the rows are rows**: they take part in indexes, reduce and the changelog, which junction
  entries were excluded from;
- **one write path**: `put` and `purge`. Nothing to pair with an `unlink`.

## Alternatives rejected

- **`owner` as the field name.** Reads as "which user owns this channel", and a channel row
  does not store a user. The user is in the membership, not the row.
- **Merging `app` into `scene`.** Loses the distinction between a machine-checked filter key
  and a human label, and makes "exactly one channel for this user and app" depend on how
  somebody spelled a label.
- **A numeric `uid` column on prism's user row.** A second identity for the same person, with
  a conversion at every boundary and two numbers to keep in step. `Uuid7` is already
  fixed-width, which was the only argument for a number.
- **A channel-scoped member key** (`members` keyed `[channel_id][member_id]`). It makes the
  same person two rows with two names, and leaves "who is this uuid" unanswerable without a
  channel. Rejected once identity became global: the channel belongs to the relationship.
- **Filtering in the caller.** The invariant would then be stated twice — once in each
  application — instead of at the storage line that owns the rows.

## History

This ADR is the terminal state. The changes it records:

1. **`scene` alone** (`863de61`): the first cut of the row was `{ scene, created_ms }`, from
   "a channel can carry data, e.g. the application scenario". It had no owner: the field was
   a label and nothing filtered on it.
2. **a `memberships` mirror** (same change): `members` could only be read by channel, and the
   application needed the other direction.
3. **`owner` proposed as the DEVICE id** — withdrawn: the device is what prism resolves
   *from*, not what a channel belongs to.
4. **`owner` renamed to `app`** (`7df7508`), and kept apart from `scene`. The rejected name is
   recorded above rather than in a later patch to this ADR.
5. **`member_id`: `u64` → `Uuid7`** (`19966bf`): the first cut used a numeric member id, and
   the chain device → user → channel could not carry prism's identity through it.
6. **An app table** (`25c5c16`): `app` started as a `String` on the channel row and became
   the `Uuid7` `app_id` pointing at `apps`. A filter key spelled by hand made "the one
   gravity channel" depend on a spelling.
7. **The member stops being channel-scoped, and the mirror table disappears** (this change):
   the first cut keyed `members` by `[channel_id][member_id]` and kept `memberships` as its
   mirror, with `since_ms` as the only thing a relationship could carry. Identity is global,
   so the channel prefix left the member key; the relationship then became a table in its own
   right — `{ nickname, since_ms }` plus a `by_member` index — which is what made the mirror
   unnecessary. The graph followed: a member node is `member-{uuid}`, no longer
   `member-{channel}-{uuid}`.
