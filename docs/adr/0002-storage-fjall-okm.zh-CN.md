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

graph-memory 设计（wiki）已给出原子图的 KV 编码模式；剩余的 flat 记忆、
会话、任务表都是 CRUD 形状，直接映射。

## 决策

**Fjall 为存储引擎。所有 schema 用 OKM derive 宏声明。**

- 身份字段（`session_id`、`user_id` 散列为 u64、表 ns）用定宽 key——
  编译期 `KEY_LEN`，hex stability test 锁布局。
- Payload 走 `RowEncode` 的 TLV；变长内容（消息文本、事实摘要）作为
  String payload 字段，永不进 key。
- 会话消息：key `(ns, user_id, session_id, seq)`——排序用每会话单调
  seq，与插入同一 WriteBatch 内分配（不用墙钟时间戳：同一毫秒的两条
  消息不能乱序）。prefix checkpoint 模型要求稳定全序。
- 任务：`(task_id, seq)` 复合主键沿用旧 PG schema；task_links DAG 边走
  OKM `EdgeTable`（方向位头）。
- 原子事实图：`edge:{src}:{label}:{dst}` 按 wiki 编码，正反向邻接用
  OKM `EdgeTable`。
- 一个 WriteBatch = 一个事务：checkpoint 写入 + 消息截断 + 事实入库 +
  权重 delta 原子提交，要么全落要么全不落。
- SlateDB+S3 后端藏在同一 engine trait 后面，等跨机共享成为真实需求
  再启用（与 mem-server 同一个 gate）。

## PG 退场与迁移

- **导出**：从 PG 一次性 JSONL dump（`memories`、`session_messages`、
  `session_checkpoints`、`tasks`、`task_items`）。
- **导入**：只走正常写 API——不开第二写入通道（与 OKM parquet restore
  path 同一纪律）。
- **遗留 flat 记忆保持 flat。** 它们没有三元组结构，强行图谱化等于
  编造关系。原样载入 Phase 1 的 flat 表；只有新知识走提取管线。
  旧数据旧形态。
- **质量 gate**：新核心上的混合检索在固定 query 集上追平或超过 PG 检索
  （以召回为准；排序允许不同）之前，Phase 1 不算完成。

## 后果

- plpython3u 触发器架构随 PG 退役；embedding 移入进程（ADR-0003）。
  API key 单一来源问题从"PG 进程环境"简化为普通进程环境变量。
- Schema 演化走 OKM 的 versioned enum 路径，不是 DDL migration。
- 运维面收缩：无 PG 实例、无 ParadeDB 镜像、无扩展升级——磁盘上一个
  目录。
- Fjall 单写者模型在单 agent 规模下可接受；多 agent 共享记忆的未来
  由 SlateDB+S3 承接，不与 Fjall 硬掰。
