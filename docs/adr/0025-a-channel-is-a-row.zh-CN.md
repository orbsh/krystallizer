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

**1. 频道是一行记录**（`channels`，ns 45）：`[channel_id]` → `{ app, scene, created_ms }`。

- `app` 回答**哪个应用拥有这个频道**——OIDC 的 `client_id` 语义。由部署写入，永不改标，因为它是过滤键：上面那个「恰好一条」的不变量就靠它校验。
- `scene` 回答**这个频道是干什么的**，用写入方自己的话（`private`、`group:ops`）。刻意是自由文本；用闭合枚举会让每个新场景变成一次迁移。

两者分开，因为归属方和生命周期不同：机器过滤的键，不能因为人改了个标签就变。

**2. 成员关系是两张表，一次提交写入**（`members`、`memberships`）。

`members`（既有，键 `[channel_id][member_id]`）回答「谁在这个频道里」，并携带频道内的身份行：`display_name`、`role`。`memberships`（ns 46，键 `[member_id][channel_id]`）是它的镜像，回答「这个成员有哪些频道」，一次前缀扫描。`join()` 在一个 batch 里写两行，只提交一次。

**3. 过滤在存储层做。** `channels_of_app(app)`、`channels_of(member)`，以及门 `GET /channels?app=`、`GET /members/{id}/channels?app=`。成员关系行自身没有 app：过滤施加在它指向的频道行上。

**4. 成员 id 就是用户 id，类型是 `Uuid7`。** 由 prism 铸造；k10r 把同一个值放进 `MemberKey`、`MembershipKey`、`CursorKey` 和消息日志的 sender。不铸数字代理键、不对 uuid 取哈希：日志里一个回不到身份的 id，会让之后每个「这是谁写的」都无解；而 `Uuid7` 是 128 位定宽值，键布局仍然定宽——只是更宽。成员行仍然是频道内身份：同一个人在两个频道里是两行，各自有自己的 `display_name` 和 `role`。这也是它不叫 user 的原因。

## 为什么不用 junction

okm 的 `Junction`（okm ADR-0015）是裸多对多的正确载体，这里不合适，两个理由来自同一个性质——junction 条目的值是**空**的：

- 频道内的身份行（`display_name`、`role`）总得有地方放，放在 junction 旁边就是一个事实两次写。
- 反向查询无论如何需要索引。junction 的两条条目给的是它被问的那个方向，但要把结果配上频道**行**，每个命中还要再查一次；镜像表让第二个方向直接是一次前缀扫描。

值得从 junction 学的、也确实学了的那一点：双写是**一次**提交，所以两个方向不可能只存在一个。`join()` 就是那次提交。

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
