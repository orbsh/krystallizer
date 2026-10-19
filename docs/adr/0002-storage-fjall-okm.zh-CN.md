# ADR-0002: 存储 — Fjall + OKM 声明式编码，PG 退场与迁移

**状态**：Accepted（设计定案，实现未开始）
**日期**：2026-09-08
**英文版**：[0002-storage-fjall-okm.md](0002-storage-fjall-okm.md)

## 背景

旧 mem 模块跑在 PostgreSQL（ParadeDB）上，embedding 由 plpython3u 触发器
在 PG 进程内计算。那套架构的存在理由是"把计算放到离数据最近的地方"——
数据在 PG，所以 embedding 在 PG 内算，1 RTT。一旦整个记忆系统移入应用
进程（Rust），这个权衡反转：离数据最近的地方**就是进程本身**，PG 只会
增加网络跳、runtime DDL 锁、SQL 解析税和插件版本耦合（pgvector /
pg_search）——而数据量级是每用户几万条语料。

graph-memory 设计（wiki）已给出原子图的 KV 编码模式；剩余的 channel 日志、
任务表都是 CRUD 形状，直接映射。（flat 记忆也是，且在图成为载体后已撤除——
2026-10-19。）

## 决策

**Fjall 为存储引擎。所有 schema 用 OKM derive 宏声明。**

- 身份字段（`user_id`、`channel_id`、表 ns）用定宽 key——编译期
  `KEY_LEN`，hex stability test 锁布局。
- Payload 走 `RowEncode` 的 TLV；变长内容（消息文本、实体名）作为
  String payload 字段，永不进 key。
- 消息（ns 33）：key `(ns, channel_id, ts_ms + 决胜段)`——排序用 gravity
  打的时间戳（ADR-0008）。回复可证明晚于它所回应的内容，所以物理时钟能
  保住因果序，而共享 seq 计数器不能；同一毫秒的并发用决胜段分开。
  prefix checkpoint 模型要求的稳定全序照旧成立，checkpoint 本身
  （ns 34，按成员）不可变。
- 任务与事实图是**同一张图**（ADR-0001：第三个接口面与事实同图——任务
  顶点 + `task_links` 依赖边）。旧 PG schema 的 `(task_id, seq)` 复合主键
  留档为迁移映射。
- 原子事实图：一个 OKM 边集合 + 声明式面，按 ADR-0010 §1——实体是节点
  集合（ns 36），事实是边集合（ns 37），边体带 `predicate_id` 与声明字段
  `since` / `confidence` / `weight`，另有种类面与遍历面。**不是**
  `edge:{src}:{label}:{dst}` 那种键布局，而 `EdgeTable` 是个旧名：上面那层
  是 `Graph<S, E: KvGraph>` + `NodeRef` + 种类字典（okm ADR-0017）。
- **追加优先，不是只追加——按表分范围。** channel 日志（ns 33）无例外地
  只追加：提取、重放、审计都读它，而一个可以被更正的日志会同时失去这三者。
  图与数据面是追加优先：节点行/边行、权重、派生索引项、code 行，在「禁止
  修改比修改本身更贵」的地方可以被更正——活的例子是 code store：它的
  `put(id)` 重写以该 id 标识的一行。追加成为默认的理由是**更简单**（无读改
  写竞态、可重放、坏派生只是一次重试），不是因为更纯。
- 一个 WriteBatch = 一个事务：checkpoint 写入 + 事实入库 + 权重 delta 原子
  提交，要么全落要么全不落。
- 没有任何东西截断日志：压缩点住在自己的表里（ns 34），用 `coverage_ts`
  指向日志——那里的一行是 latest-wins，这种可变性被关在它自己的表内。
  坏摘要是一次重试，不是损失。
- 存储永不分裂成两个真相：视图裁剪只在读侧，且谁拥有写谁拥有记录。
- SlateDB+S3 后端藏在同一 engine trait 后面，等跨机共享成为真实需求
  再启用（与 mem-server 同一个 gate）。

## PG 退场与迁移

- **导出**：从 PG 一次性 JSONL dump（`memories`、`session_messages`、
  `session_checkpoints`、`tasks`、`task_items`）。
- **导入**：只走正常写 API——不开第二写入通道（与 OKM parquet restore
  path 同一纪律）。
- **遗留 flat 记忆保持 flat——而它们导入的那张表已撤（2026-10-19），导入目标由迁移决定。** 它们没有三元组结构，强行图谱化等于
  编造关系。原样载入 flat 表——那是长期记忆在图谱化之前的载体，随图一起
  撤掉（ADR-0001；PLAN Phase 3）。撤掉之后那些导入的旧行怎么处置，是一个
  迁移问题，不是 schema 问题。
- **质量 gate**：新核心上的检索在固定 query 集上追平或超过 PG 检索
  （以召回为准；排序允许不同）之前，Phase 1 不算完成。按 ADR-0003，比的是
  端到端检索质量，不再逐分支对 pgvector + pdb.bm25 + RRF 比召回（PLAN 1d）。

## 后果

- plpython3u 触发器架构随 PG 退役；embedding 移入进程（ADR-0003）。
  API key 单一来源问题从"PG 进程环境"简化为普通进程环境变量。
- Schema 演化走 OKM 的 versioned enum 路径，不是 DDL migration。
- 运维面收缩：无 PG 实例、无 ParadeDB 镜像、无扩展升级——磁盘上一个
  目录。
- Fjall 单写者模型在单 agent 规模下可接受；多 agent 共享记忆的未来
  由 SlateDB+S3 承接，不与 Fjall 硬掰。