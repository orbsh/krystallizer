# 0025 — A channel is a row, and membership reads both ways

> **Languages:** [English](0025-a-channel-is-a-row.md) (primary) · [中文](0025-a-channel-is-a-row.zh-CN.md)

**Status:** Accepted (2026-11-04). §§1–3 landed in mem-core and the HTTP surface
(`channels` ns 45, `memberships` ns 46, four doors, `?app=`). §4 is the decision on
identity; its key migration (`member_id` `u64` → `Uuid7`) is the change in flight, and
the decision is final regardless of that schedule.
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

**1. A channel is a row** (`channels`, ns 45): `[channel_id]` → `{ app, scene, created_ms }`.

- `app` answers *which application owns this channel* — the OIDC `client_id` question. It is
  written by the deployment and never relabelled, because it is a filter key: the "exactly
  one" invariant above is checked against it.
- `scene` answers *what the channel is for*, in the writer's own words (`private`,
  `group:ops`). Free text on purpose. A closed enum would make every new scene a migration.

The two are separate fields because they have different owners and different lifetimes: a
key the machine filters on must not change when a human renames a label.

**2. Membership is two tables, written in one commit** (`members`, `memberships`).

`members` (existing, keyed `[channel_id][member_id]`) answers "who is in this channel" and
carries the channel-scoped identity row: `display_name`, `role`. `memberships` (ns 46, keyed
`[member_id][channel_id]`) is its mirror and answers "which channels this member has" as one
prefix scan. `join()` writes both rows in a single batch and commits once.

**3. Reads are filtered at the storage line.** `channels_of_app(app)`,
`channels_of(member)`, and the doors `GET /channels?app=`, `GET /members/{id}/channels?app=`.
The membership row carries no app of its own: the filter is applied to the channel row that
the membership names.

**4. The member id IS the user id, and it is a `Uuid7`.** prism mints it; k10r stores the
same value in `MemberKey`, `MembershipKey`, `CursorKey` and the message log's sender. No
numeric surrogate, no hash of the uuid: an id in a log that cannot be turned back into the
identity it names is a dead end for every later question ("who wrote this"), and `Uuid7` is a
fixed-width 128-bit value, so the key layout stays fixed-width — only wider. The member row
remains channel-scoped: the same user in two channels is two rows, each with its own
`display_name` and `role`. That is why the row is not called a user.

## Why not a junction

Channel and member ARE a many-to-many, and okm's `Junction` (okm ADR-0015) is exactly the
carrier for one — so the difference is not the direction, it is the PAYLOAD.

A junction entry has a key and no value: `link(a, b)` writes two entries in one batch (one
under each endpoint's ns) and both are empty-valued. ADR-0015 says what that costs: "The
entry value is empty — it cannot carry relation attributes (join time, role, permission), the
classic need of a SQL junction table". Every pair here carries data — the name this person
has IN THIS channel (`display_name`), whether they are a person or the agent (`role`), when
they joined (`since_ms`) — and an empty entry has nowhere to put it.

So the choice is a junction plus a side document addressed by the endpoint pair (one join =
three writes, one fact stated twice) or two document tables (two rows that ARE the data, one
of them keyed so its order IS the reverse index). The data has to live in a table either way,
so the second shape has one representation fewer.

What is worth taking from the junction, and is taken: the double write is ONE commit, so
neither direction can exist alone. `join()` is that commit.

(An earlier revision of this section argued that "the reverse query needs an index anyway".
Withdrawn: a junction is bidirectionally materialized, so its reverse is a prefix scan, and
the mirror table's read has to fetch the channel row per hit just the same — that was never
where the two paths divide.)

## Alternatives rejected

- **`owner` as the field name.** Reads as "which user owns this channel", and a channel row
  does not store a user. The user is in the membership, not the row.
- **Merging `app` into `scene`.** Loses the distinction between a machine-checked filter key
  and a human label, and makes "exactly one channel for this user and app" depend on how
  somebody spelled a label.
- **A numeric `uid` column on prism's user row.** A second identity for the same person, with
  a conversion at every boundary and two numbers to keep in step. `Uuid7` is already
  fixed-width, which was the only argument for a number.
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
5. **`member_id`: `u64` → `Uuid7`** (in flight): the first cut used a numeric member id, and
   the chain device → user → channel could not carry prism's identity through it.
