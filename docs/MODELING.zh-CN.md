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
| 33  | Channel 消息日志   | 已实现（最小切片，2026-10-09） | ADR-0008 |
| 34  | Checkpoint（每成员）| 已实现（最小切片，2026-10-09） | ADR-0008 |
| 35  | Code（交付的脚本） | 已实现（bring-up 切片，2026-10-15） | gravity ADR-0001/0003 |
| —   | Session 消息      | 被 channel log 取代      | ADR-0008 |
| —   | Tasks / task_links DAG | 已声明（ADR-0001——第三个接口面；与事实同图） | ADR-0001, ADR-0002 |
| 36  | 实体（事实图）     | 已实现（2026-10-17）      | ADR-0010 |
| 37  | 事实（事实图边）   | 已实现（2026-10-17）      | ADR-0010 |
| —   | 初始化记忆（人设 / 风格） | 已声明（ADR-0001） | ADR-0001 |

图的读侧（LPA、分箱、胶囊）、会话日志缺的几块、以及任务，属后续阶段；
上表标 `已声明` 的行是 ADR 承诺，非运行代码。

## 记忆种类（谁发起）

三种，按谁驱动分（ADR-0001）；命名是 被动 / 主动，
短期 / 长期 作内容时程的定语：

- **被动（短期）**——当前上下文：channel 日志（ns 33）+ 每成员投影
  （ns 34）。循环自己取、自己追加，从不作为工具。
- **主动（长期）**——不在上下文里的事实，以**工具**形式到达，任何 agent 都能
  用。载体是 fact graph；flat 记忆（ns 32）是它在图谱化之前的形态——同一份知识以
  文本行存放，所以图落地时它一并撤掉（文本块没有逐条身份：按条更新、加权、枚举都
  做不了）。原始内容从来不需要它——channel 日志（ns 33）已经收了每一条消息。
- **初始化（人设 / 风格）**——图派生：从用户节点出发遍历组装，投到
  投影头部。尚未分配 ns。

## Flat memory（ns 32）

已实现（Phase 1a）——抽取出的长期记忆在图谱化之前的载体，图落地时撤掉（ADR-0001 的「flat memory, then the graph」；PLAN Phase 3）。声明在 `crates/mem-core/src/memory.rs`。

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
  态；Phase 1 增加内置本地 embedding 模型（向量存为 `Vector<f32>`
  帧，从内存表检索）+ 名/别名点查 + weight 元数据（ADR-0005）。词法
  分支（jieba / 倒排索引 / BM25 / RRF / arroy）已撤销——ADR-0003。

### 身份生成

`id` 是 `MemoryStore` 分配的自增代理键。计数器不持久化：`open()` 扫
描表序最后一个 key 恢复（`[user_id][id]` BE → 末位 key 携带全局最大
id），从其之后继续。后果：id 唯一但不按 user 连续，计数器跨 user 全
局——不要假设 per-user 的 id 密度。

## 访问方法清单

OKM 强制纪律：每个实体的查询必须是声明的访问方法，不是扫描。现状：

- Flat memory（ns 32）：只有 user 前缀扫描（key 前缀）。这张表是长期记忆的
  **图前**载体，随图一起撤销（ADR-0001，PLAN Phase 3），所以 ADR-0003 描述的
  向量落址与名/别名点查声明在图的两个 collection 上，不在这张表上。
- Channel 消息（ns 33）：`[channel_id][ts_ms][sub]` 前缀扫描——回放、
  increment、max-gap 读都是一次前缀扫描（ADR-0008 §3）。checkpoint（ns 34）：
  按 `[channel_id][member_id]` 点读。
- Fact graph：OKM 节点 collection（entities）+ 边 collection（facts）带声明面，
  见 ADR-0010 §1——不是 `edge:{src}:{label}:{dst}` 那种键布局（`EdgeTable` 是
  旧名：上面那层是 `Graph<S, E: KvGraph>` + `NodeRef` + kind 字典，okm
  ADR-0017）。weight 二级索引 = 边 collection 的 `weight` 声明字段的面
  （ADR-0005、ADR-0010 §1）。

## Hex 稳定性

Key 布局由 `key_layout_hex_stability` 锁定
（`crates/mem-core/tests/smoke_test.rs`）：`[user_id 8B BE][id 8B BE]`，
不含 ns 头（`KeyEncode::encode()` 只返回 payload，不含 ns；Table 自
行前置 `[ns 2B][slot 1B]`）。

## Channel 日志（ns 33）+ checkpoint（ns 34）

统一 channel 日志与 agent 投影（ADR-0008，服务 gravity Phase 0 的最小切片）。
声明在 `crates/mem-core/src/channel.rs`。

### 键

- `MessageKey`（ns 33）：`[channel_id 8B][ts_ms 8B][sub 2B]`——BE 定宽，
  字节序 == `(ts_ms, sub)` 序；日志即索引（无排序步骤、无二级索引）。
  `sub` = 同毫秒 tiebreaker（单写者顺序追加；u16 溢出 = 一毫秒 65 536 条，
  病态信号）。ns/slot 在条目入址时拼接（ADR-0011）。
- `CheckpointKey`（ns 34）：`[channel_id 8B][member_id 8B]`——每
  (channel, member) 一条 checkpoint，**覆盖写**（同键 RMW）：投影只读最新
  一条；压缩决策的审计史属于 Phase 2.6 的 decision log（另一张表）。若
  未来重切分需要 checkpoint 历史，本表改为按 coverage point 键控的
  append-only 日志——pre-production schema 变更，零迁移（ADR-0018
  clean-break 规则）。

### 行

- `MessageRow`：`role u8`（user/assistant/tool/system）、`kind u8`
  （text；tool_call/tool_result 预留给视图层剪枝）、`content String`。
  存储永远持有完整行；剪枝在视图侧（ADR-0006）。
- `CheckpointRow`：`coverage_ts u64`（投影增量严格晚于此点）、
  `summary String`。

### 访问面（mem-core `ChannelStore`）

`append(channel_id, ts_ms, sub, role, kind, content)`——时钟由调用方
（gravity）持有；`fetch_full(channel_id, member_id)` →
`SessionView { coverage_ts, summary, increment }`——投影：checkpoint +
严格晚于 coverage 的增量，键序；`append_checkpoint(channel_id, member_id,
coverage_ts, summary)`。

尚未落地（后续切片）：thread、topic tag、members、读游标、压缩策略
引擎（ADR-0008/0009）。

## Code 存储（ns 35）

交付脚本的家——执行方的调用会读的那唯一一张表。声明在
`crates/mem-core/src/code.rs`；设计在 gravity（ADR-0001/0003：节点 mount
读的脚本以调用参数形态走），本仓为这张表自己的 ADR 仍欠。

### Key：`CodeKey`（`KeyEncode`）

```rust
#[derive(KeyEncode, ...)]
#[ok_ns(35)]
pub struct CodeKey {
    pub code_id: u64,
}
```

物理布局：`[code_id 8B BE]`——只有身份。没有 name，也没有内容哈希：
调用方每次调用都重读这一行，脚本以参数形态走，所以没有会被读旧的缓存
（gravity ADR-0003「Code identity」）。

### Row：`CodeRow`（`DocumentEncode`）

```rust
#[derive(DocumentEncode, ...)]
#[ok_ref(CodeKey)]
#[ok_ns(35)]
pub struct CodeRow {
    pub language: String,
    pub description: String,
    pub parameters: String,
    pub source: String,
}
```

- **一行是一段可复用的代码，不是 skill。** 一个「登录」脚本写一次，很多
  skill 都有一道步骤指向它——所以行上带 `description` 与参数定义
  （`parameters`，以文本形态承载的 JSON 文档：store 不解析它，投递这段
  代码的调用方拥有它的形态）。无论是把 skill 关联到这段代码，还是之后读
  图谱，判断依据都在行上，不必挨个读源码才知道它要什么参数。这两个字段
  都不是给 LLM 搜索用的面：**一个 skill 是什么**由图谱持有（有结构的节
  点：前置条件／流程步骤／经验／后置验证）。
- **语言是行上的数据。** 脚本即数据，数据带语言；没有任何组件替它默
  认。调用方按该语言把调用路由到对应的 mount，没有 mount 的语言是调用
  方的错误值——绝不在这里兜底。
- gravity 是唯一客户（ADR-0001：节点永不与 krystallizer 说话；字节路径
  是 gravity → 控制路径 → 节点）。

### 访问面（mem-core `CodeStore`）

`put(language, description, parameters, source)` → store 分配的
`code_id`（代理键，重启后扫末键恢复，与 flat memory 同规则）；
`get(code_id)` → 该行，未知 id = 不存在；`list()` →
`(code_id, language, description)` 按 id 序——解析方或关联方挑选的依据，
也是声明的 bring-up 姿态（全表扫描，由图谱检索取代，ADR-0003/0018）。
`list()` 不带 source 与参数定义。

## 事实图（ns 36 实体，ns 37 事实）

主动面（长期）的载体：一个 okm 节点集合 + 一个 okm 边集合，以及把一条事实落地
的写时消解（ADR-0010 §1；2026-10-17 落地）。声明在
`crates/mem-core/src/graph.rs`。

### 实体（ns 36）——节点集合

```rust
#[derive(KeyEncode, ...)]
#[ok_ns(36)]
pub struct EntityKey {
    pub scope: u64,
    pub node_id: u64,
}

#[derive(DocumentEncode, ...)]
#[ok_ref(EntityKey)]
#[ok_ns(36)]
#[ok_index(
    by_kind { fields(kind_id) },
    by_name { fields(name) },
)]
pub struct EntityRow {
    pub kind_id: u16,
    pub literal_num: i64,
    pub name: String,
}
```

- 键 `[scope 8B BE][node_id 8B BE]`——`scope` 打头是因为语料按用户划分
  （ADR-0003），与 flat memory 同一条归属边界。id 是 store 发放的代理键，重开
  时从最后一个键恢复。
- **面是「payload 字段在前、主键在后」**：key 尾由 okm 自己拼接，而派生只接受
  payload 字段（把 key 字段写进 `fields(...)` 是编译错误）。条目落成
  `[kind_id][scope][node_id]` 与 `[name][scope][node_id]`。
- **文本优先（text-first）形态**：`name` 变长且排在索引字段末位，所以按
  `alpha` 前缀扫描也会扫到 `alphabet`。精确查找因此要同时核验行里的 name
  与解出的 key 的 scope——这是把名字留在键之外要付的价。
- `name` 是**属性**（永不进键）；`kind_id` 走本集合自己的 0x2/0x3 字典分配
  ——与动态字段名共用同一本词汇表（无害：一个 id 只会从它被写进的那个字段里
  读回来）。
- `literal_num` 是字面量节点的整数投影（其余为 0）：声明的、定宽的、有序的字
  段，于是将来的数值读仍然是对某个面做 range scan，而不是向量距离的猜测。
- **实体属性是合并的。** 一个词项的属性（元组的 head/tail 属性）落在节点行的
  **动态段**（slot 1）——那是这个实体的属性，被关于它的每条事实共享——而且是
  合并：同名属性取较新的值，其余存活。
- **`community_id` 与它的 `by_community` 面尚未声明。** 它们属于 ADR-0010 的
  步骤 3–4（增量 LPA + 分箱），而今天没有任何东西读它们：没有写入者的字段和
  没有扫描者的面，代价由每条记录承担，所以它们随消费它们的读路径一起落
  （PLAN Phase 4/5）。ADR-0010 §1 保留设计形态。

### 事实（ns 37）——边集合

一个 okm 边集合（`Graph<FjallStore, FactEdge>`），每图一个 ns（okm ADR-0017）。
派生提供 `NS_PREFIX`、`ATTR_SLOTS`、`attrs()` 与每个声明字段一个 0x1 面；主表与
四个遍历面由 okm 自己造。

| 面 | 键 |
|---|---|
| 0x0 主表 | `[edge_id 8B BE]` → `[src NodeRef][dst NodeRef][kind_id][attrs]` |
| 0x1 声明字段（`since` / `confidence` / `weight`） | `[字段值][edge_id]` |
| 0x4 种类索引 | `[kind_id][edge_id]` |
| 0x5 出边 | `[src NodeRef][edge_id]` |
| 0x6 入边 | `[dst NodeRef][edge_id]` |
| 0x7 种类+出 | `[kind_id][src NodeRef][edge_id]` |
| 0x8 种类+入 | `[kind_id][dst NodeRef][edge_id]` |

- **这一侧没有 scope 段。** 边集合自己造键（okm 的边形态），所以归属边界走
  **端点**：`NodeRef` 是 `[ns 2B][len varint][实体键]`，实体键里带着 scope。
  遍历从带 scope 的节点出发，也就一直在 scope 内；只有整 ns 的扫描（0x4，
  「某种类的全部边」）跨 scope，按端点过滤是调用方的事。要在边键上带
  per-scope 前缀，就得绕开 okm 的 `Graph` 手写这个集合。
- 三个声明字段是关系 props 的**投影**：`since`（调用方的时钟——裁决输入）、
  `confidence`（写侧的；255 = 缺省，即不惩罚）与 `weight`（权重系统的，
  ADR-0005——在该系统写它之前为 0）。它们被排除在边的动态条目之外：一个数据
  只有一个家。
- **关系的开放 props 走边的动态段**（边 ns 的 slot 1），用 okm 的
  `put_frame_named` 构造，与 link **同一批**提交（ADR-0002：一个 WriteBatch
  = 一个事务）。按断言给的 `functional` 标志留在那里——这是该断言自己关于
  「它如何声明那个种类的基数」的记录。
- **`from_attrs` 是 k10r 自己的**——`[since u64][confidence u8][weight u32]`，
  13 字节，按派生声明的顺序。`EdgeEncode` 派生只发写的那一半，所以读侧自带宽
  度，并由测试对着 `attrs()` 锁死。
- **字典纪律（一个 ns 两套词汇）。** 边 ns 里住着关系种类（由 okm 的 `Graph`
  用它自己长驻的 `DictCache` 分配）**和** k10r 分配的 prop 名。两个 cache 在
  同一本字典里分配，会把同一个 id 发给两个不同的名字，帧随后以错误的名字读回
  来，而且无声。所以 k10r 分配一律走一次性 cache，边 handle 每次调用现建，并
  在类型化读之前把它预热（`kind_name`）——因为 `Graph::typed_*` /
  `edges_of_kind` 只从非分配的 `by_name_get` 取答案，从不回落到引擎。欠 okm 的
  条目在 ADR-0010 的欠账清单里。

### 访问面（mem-core `GraphStore`）

`ingest(scope, fact)` 解一条 8 元组并跑 ADR-0010 §1 的步骤 1–2：两个端点经
`entity_by_name` 解析（不存在则新建，并分配它的种类），随后关系要么挂一条新
边，要么——当事实声明了 `functional`——与该源上该种类的在册边裁决（`since >`
或 `confidence >=` 胜出：同一 `edge_id` 就地改写；否则这条事实被丢弃）。返回
`IngestOutcome::{Linked, Rewritten, Dropped}`。

读：`entity(scope, node_id)`、`entity_by_name(scope, name)` → `(node_id, row)`、
`entity_kind(kind_id)` → 词项、`entity_props` / `fact_props`（动态段）、
`fact(edge_id)` → 解出的事实、`out_edges` 与 `typed_out`（遍历通货）。函数式
改写是 `unlink` + `link`——两次引擎提交，因为 okm 的 `unlink` 没有 into-batch
形态（欠账，见 ADR-0010）。

### 存储组装

一个进程一个 Fjall **database**（一个目录、一把锁文件），**两个
keyspace**：`channel`（日志是类队列的，写频率全库最高，所以单独一棵 LSM
树，它的 compaction 不会拖着别的表）与 `memory`（flat memory + code + 事实图，将来
的 fact graph 与 skill 也进这里——这些表靠 ns 区分）。
`crate::service::open_service` 打开 database，再用 `FjallStore::from_db`
把各组挂上去。Fjall 锁目录，所以对同一目录第二次 `FjallStore::open` 会失
败——keyspace 挂载就是让各组共用同一目录、同一把锁的办法。这只是进程内表
的组织方式；k10r 仍是 ADR-0007 定下的东西（独立服务、自有存储、只经 HTTP
到达）。

一个值得知道的后果：Fjall 的 batch 本身可以跨 keyspace
（`WriteBatch::insert(&keyspace, …)`，一次 commit = 一次 WAL 写），但 okm
的 `commit_batch` 会把每个 op 写进**它被调用的那个 store handle**，也就是
一个 keyspace。混着 channel op 与 code op 的批，会全部落进接住这次调用的
那个 keyspace——所以批必须留在同一个 keyspace 内（读写本来也不需要跨这两
个）。
