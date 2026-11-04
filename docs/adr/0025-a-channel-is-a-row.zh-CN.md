# 0025 — 频道是一行记录，成员关系两个方向都能读

> **Languages:** [English](0025-a-channel-is-a-row.md) (primary) · [中文](0025-a-channel-is-a-row.zh-CN.md)

**Status:** Accepted (2026-11-04)。§§1–3 已落在 mem-core 与 HTTP 面（`channels` ns 45、`memberships` ns 46、四道门、`?app=`）。§4 是身份上的决定；它的键迁移（`member_id` 由 `u64` 改为 `Uuid7`）正在进行，决定本身不受这个排期影响。
**Date:** 2026-11-04

## Context

频道原本只是别的一些表键前缀里的一个**数字**：`MessageKey` 是 `[channel_id][turn][ts][sub]`，`CheckpointKey`、`CursorKey` 同理，`members` 是 `[channel_id][member_id]`。没有任何地方说频道**是什么**，而应用真正要问的两个问题都没有答案：

- **「这个人有哪些频道？」**——`members` 以频道开头，问它是整表读，维度是错的。
- **「我自己的频道里，哪一个是这个？」**——没有东西可过滤：没有归属、没有应用、没有标注。应用只能从日志里反推。

应用侧把这个缺口变具体了。gravity 要「一个用户一个频道」：prism 把 device 解析成 user（prism ADR-0017：`device_user(DeviceId) -> Option<UserId>`），gravity 接着要找到属于它的那**唯一**一个频道。这是一个存储问题——「这个人的频道，我应用拥有的那些，期望恰好一条」——而存储层没有对应的形状。

## Decision

**1. 频道是一行**（`channels`，ns 45）：`[channel_id]` → `{ app_id, scene, created_ms }`。

- `app_id` 回答「这个频道属于哪个应用」——OIDC 的 `client_id`，就是那个 `Uuid7`，指向 `apps` 表（ns 43：`[app_id]` → `{ name, created_ms }`）。部署时写定、永不改标，因为它是过滤键：「恰好一个」这个不变量按它来校验。自由文本顶不住——`"gravity"` 两种拼法就是两个应用。
- `scene` 回答「这个频道是干什么的」，用写它的那些人的话（`private`、`group:ops`）。故意留成自由文本：一个封闭枚举会把每个新场景都变成一次迁移。

两者分开，是因为拥有者和生命周期不同：机器用来过滤的键，不能在有人改标签时跟着变。

**2. 身份是全局的，成员关系是那张关系表**（`members` ns 42，`memberships` ns 46）。

`members` 的键只有 `[member_id]`，行上是 `{ display_name, role }`——一个人一行，不管他在几个频道里。按频道切分是第一版的做法，它让「这个 uuid 是谁」必须先说出一个频道才答得上来。

`memberships` 的键是 `[channel_id][member_id]` → `{ member_id, nickname, since_ms }`，并在成员列上声明 `by_member` 索引。频道在前是因为频道是容器：删一个频道就是一段键区间的事；而「这个人在哪些频道」是索引的活。`nickname` 是频道内的名字，空 = 用身份的 `display_name`——与 checkpoint 摘要同一条「空即不存在」的规矩，因为 `DocumentEncode` 没有 `Option<T>`。`join()` 在一个批里写身份行与关系行，提交一次。

**3. 读在存储层收口。** `channels_of_app(app_id)`（按 app id 等值）、`channels_of(member_id)`（走 `by_member` 索引，每条命中一次回表）、`members(channel_id)`（关系表上的前缀扫描），以及两个门 `GET /channels?app=`、`GET /members/{id}/channels?app=`。关系行自己没有 app：过滤落在它指向的那条频道行上。

**4. 成员 id 就是用户 id，类型 `Uuid7`。** prism 铸造，k10r 原样存进 `MemberKey`、`MembershipKey`、`CursorKey`、消息日志的 sender，以及图里 `member-{uuid}` 的节点名。不用数字代理键，不哈希 uuid：日志里一个回不到身份的 id，对之后每一个「这是谁写的」都是死路；而 `Uuid7` 是 128 位定宽值，键布局照旧定宽，只是更宽。

## 为什么不用 junction

多对多就是一张普通的表，而 okm 有普通的表：两个身份组成的键，加上关系自有的那些列。`memberships` 就是这样一张表，所以这里没有 junction 这个载体——okm 那边在同一个结论被它自己的测试验证之后，已经把 `Junction` 退役了（那边 ADR-0051）。

普通表比「存一对空键」的载体多出四样：

- **关系能带数据**——`since_ms`，以及每频道的 `nickname`；
- **反向方向是声明索引**，由 `put`/`purge` 在同一个 batch 里维护，`rebuild_faces` 能重建，而不是再手写一条条目；
- **行就是行**：参与索引、reduce 与变更流，而 junction 条目是被排除在外的；
- **只有一条写路径**：`put` 与 `purge`，没有需要配对的 `link`/`unlink`。

## Alternatives rejected

- **字段叫 `owner`。** 读起来是「哪个用户拥有这个频道」，而频道行不存用户。用户在成员关系里，不在行里。
- **把 `app` 并进 `scene`。** 会抹掉机器校验的过滤键和人写标签的区别，使「这个人这个应用恰好一个频道」取决于某人怎么拼标签。
- **在 prism 的 user 行上加数字 `uid` 列。** 同一个人第二个身份，每个边界都要转换，两个数字要保持同步。`Uuid7` 本来就是定宽——那曾是选数字的唯一理由。
- **在调用方过滤。** 不变量会被陈述两次（每个应用一次），而不是在拥有这些行的存储层陈述一次。

## 历史

本 ADR 记录终态。它收录的变化：

1. **只有 `scene`**（`863de61`）：行最初是 `{ scene, created_ms }`，来自「频道可以标注数据，比如应用场景」。它没有归属：那个字段只是标签，没有东西靠它过滤。
2. **`memberships` 镜像**（同一次改动）：`members` 只能按频道读，而应用需要另一个方向。
3. **`owner` 曾被提为 device id**——撤回：device 是 prism 解析的**起点**，不是频道的归属。
4. **`owner` 改名 `app`**（`7df7508`），并与 `scene` 分开。被否决的名字记在这里，而不是日后在本 ADR 上打补丁。
5. **`member_id`：`u64` → `Uuid7`**（进行中）：最初的实现用数字成员 id，device → user → channel 这条链没法把 prism 的身份带过去。
6. **app 变成一张表**（`25c5c16`）：`app` 起初是频道行上的 `String`，现在是 `Uuid7` 的 `app_id`，指向 `apps`。手写的过滤键会让「gravity 那唯一一个频道」取决于拼法。
7. **成员不再按频道切分，镜像表随之消失**（本次改动）：第一版把 `members` 的键做成 `[channel_id][member_id]`，并留 `memberships` 当镜像，关系只能装 `since_ms`。身份是全局的，频道前缀离开成员键之后，关系自己成了一张表——`{ nickname, since_ms }` 加 `by_member` 索引——镜像也就不需要了。图跟着改：成员节点是 `member-{uuid}`，不再是 `member-{channel}-{uuid}`。
