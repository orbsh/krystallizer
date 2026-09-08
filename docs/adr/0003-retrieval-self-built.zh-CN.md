# ADR-0003: 检索自建 — arroy 向量 + 手写 BM25 + RRF

**状态**：Accepted（设计定案，实现未开始）
**日期**：2026-09-08
**英文版**：[0003-retrieval-self-built.md](0003-retrieval-self-built.md)

## 背景

旧检索路径是 ParadeDB SQL：pgvector 余弦 + pdb.bm25 全文 + RRF 融合，
单条查询。离开 PG（ADR-0002）意味着这些扩展不再可用。选项：跟着换一个
SQL 引擎、采用 Rust 搜索框架、或直接建两个索引。

## 决策

**向量：arroy（HNSW，Rust），图序列化进 Fjall，启动时载入内存。**

- 语料按 user 分；几万向量装内存绰绰有余。与既定选型原则一致：
  能装内存用 HNSW。
- 写入时按 user 增删；重建是本地操作。

**全文：自建倒排索引 + 手写 BM25。中文分词用 jieba-rs。不引 tantivy。**

- user 级语料上的 BM25 是几百行的事：词频、文档长度、IDF、打分。落在
  既定规则里——简单算法直接实现，只有复杂算法（PageRank、社区发现）
  才值得引依赖。
- wiki 明说"分词与打分全链路可控"是 KV 路线相对 pg_search 黑盒
  tokenizer 的核心换取项——引 tantivy 等于把两样都交还给一棵 Lucene
  形状的依赖树（数 MB 依赖、自带 runtime、自带索引格式），去解决一个
  我们在这个语料量级下不存在的问题。
- 倒排索引本身是 KV 形状（term → postings），与其他数据同住 Fjall；
  不引入第二个存储引擎。

**融合：进程内 RRF。** 旧 SQL CTE（两个排名分支、RRF 权重、join）变成
普通函数——同样的 k=60 RRF，一次函数调用替代一次查询。

## 后果

- "functions without boundaries" 在主场兑现：过去是 PG 内 UDF
  （plpython3u embedding）和 SQL CTE（混合检索）的东西，现在是普通
  库代码，性能贴近数据库内核。
- Embedding 调用：若 embed 模型仍是远端 API，写入/查询仍有那一跳
  HTTP——与 PG 方案持平（PG 转发同一跳）。归零需本地 embed 模型，
  属独立决策，不在本文档范围。
- 兜底姿态：每 user 边数低于约 1 万时，bring-up 阶段可用暴力扫描
  顶替 arroy；arroy 集成不阻塞 Phase 1。
- 调参钩子（分词器选择、BM25 k1/b）是代码层常量，不是黑盒。
