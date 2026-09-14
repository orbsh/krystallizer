# Krystallizer 数据建模

memory core 的 schema 集中一处：现有哪些表、key 与 row 形态、声明了
哪些访问方法。OKM 通用建模规则（四层、访问方法纪律、键尾规则）不在
此重复——见 okm 的
[MODELING.zh-CN.md](../../../okm/docs/MODELING.zh-CN.md)，同样对本仓
规范生效。每张表背后的设计决策见各 ADR。

> **Languages:** [English](MODELING.md) · [中文](MODELING.zh-CN.md)（主文档）

## 表

一个实体类 = 一个 ns（OKM 规则）。现状：

| ns  | 实体              | 状态                     | ADR      |
|-----|-------------------|--------------------------|----------|
| 32  | Flat memory       | 已实现（Phase 0）        | ADR-0002 |
| —   | Session 消息      | 已声明（ADR-0002）       | ADR-0001, 0002 |
| —   | Tasks / task_links DAG | 已声明（ADR-0002）  | ADR-0002 |
| —   | Fact graph 边     | 已声明（ADR-0002）       | ADR-0002 |

Session、tasks、fact graph 属后续阶段；下文形态是 ADR 承诺，非运行
代码。

## Flat memory（ns 32）

唯一已实现的表。声明在 `crates/mem-core/src/memory.rs`。

### Key：`MemoryKey`（`KeyEncode`）

```rust
#[derive(KeyEncode, ...)]
#[kv_ns(32)] // 历史原因：声明在 key 上（row 声明点之前）
pub struct MemoryKey {
    pub user_id: u64,
    pub id: u64,
}
```

物理布局：`[user_id 8B BE][id 8B BE]` = 16 字节，零填充。`user_id`
是组织前缀（user 范围扫描）；`id` 是代理身份——属性是 payload，不
进 key。

### Row：`MemoryRow`（`RowEncode`）

```rust
#[derive(RowEncode, ...)]
#[kv_ref(MemoryKey)]
#[kv_ns(32)] // row 是表的声明点
pub struct MemoryRow {
    pub text: String,       // cold TLV 段
    pub created_at: u64,    // hot 段（定宽）
}
```

- ns 32 声明在 row 上（`#[kv_ns]`）；key 类型不携带 ns（新版 OKM
  API：`Table::new(store)` 不收 ns；组装用 `MemoryRow::table(store)`）。
- Payload：定宽字段连续排列（hot），变长 text 走 TLV（cold）。布局
  版本默认 1。
- 尚无 `#[kv_index]` 访问方法——检索是 Phase 0 的暴力 substring 扫描
  （`scan_keys` + 过滤）。这是声明的 bring-up 姿态（ADR-0003），非终
  态；Phase 1 增加 BM25 倒排索引（token 上的 function index）+ arroy
  向量 + weight 元数据（ADR-0005）。

### 身份生成

`id` 是 `MemoryStore` 分配的自增代理键。计数器不持久化：`open()` 扫
描表序最后一个 key 恢复（`[user_id][id]` BE → 末位 key 携带全局最大
id），从其之后继续。后果：id 唯一但不按 user 连续，计数器跨 user 全
局——不要假设 per-user 的 id 密度。

## 访问方法清单

OKM 强制纪律：每个实体的查询必须是声明的访问方法，不是扫描。现状：

- Flat memory：未声明（暴力扫描是 Phase 0 姿态）。Phase 1 承诺：
  user 前缀扫描（key 前缀）、token function index（BM25 postings）、
  weight 二级索引 `W:{weight}:{edge_id}`（ADR-0005）。
- Session 消息：`[user_id][session_id]` 前缀扫描，`seq` 在 key 尾
  ——回放/范围读就是一次前缀扫描（ADR-0002）。
- Fact graph：`edge:{src}:{label}:{dst}` + EdgeTable 双向邻接
  （ADR-0002），weight 索引见 ADR-0005。

## Hex 稳定性

Key 布局由 `key_layout_hex_stability` 锁定
（`crates/mem-core/tests/smoke_test.rs`）：`[user_id 8B BE][id 8B BE]`，
不含 ns 头（`KeyEncode::encode()` 只返回 payload，不含 ns；Table 自
行前置 `[ns 2B][slot 1B]`）。
