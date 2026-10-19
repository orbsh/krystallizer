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
| 32  | Flat memory       | **2026-10-19 撤除**（下面一块是历史） | ADR-0002 |
| 33  | Channel 消息日志   | 已实现（最小切片，2026-10-09） | ADR-0008 |
| 34  | Checkpoint（每成员）| 已实现（最小切片，2026-10-09） | ADR-0008 |
| 35  | Code（交付的脚本） | 已实现（bring-up 切片，2026-10-15） | gravity ADR-0001/0003 |
| —   | Session 消息      | 被 channel log 取代      | ADR-0008 |
| —   | Tasks / task_links DAG | 已声明（ADR-0001——第三个接口面；与事实同图） | ADR-0001, ADR-0002 |
| 36  | 实体（事实图）     | 已实现（2026-10-17）      | ADR-0010 |
| 37  | 事实（事实图边）   | 已实现（2026-10-17）      | ADR-0010 |
| 38  | 社区序号（发号用，每 scope 一行） | 已实现（2026-10-20） | ADR-0010 |
| —   | 初始化记忆（人设 / 风格） | 已声明（ADR-0001） | ADR-0001 |

图的读侧（LPA、分箱、社区的渲染）已随之落地（2026-10-19/20）；会话日志缺的几块、
任务、以及 scope 级 Top-K 需要的 `[scope][weight][edge_id]` 投影表，属后续阶段；
上表标 `已声明` 的行是 ADR 承诺，非运行代码。

## 记忆种类（谁发起）

三种，按谁驱动分（ADR-0001）；命名是 被动 / 主动，
短期 / 长期 作内容时程的定语：

- **被动（短期）**——当前上下文：channel 日志（ns 33）+ 每成员投影
  （ns 34）。循环自己取、自己追加，从不作为工具。
- **主动（长期）**——不在上下文里的事实，以**工具**形式到达，任何 agent 都能
  用。载体是 fact graph；flat 记忆（ns 32）**曾经**是它在图谱化之前的形态——同一份
  知识以文本行存放，所以图落地时它一并撤掉了（文本块没有逐条身份：按条更新、加权、
  枚举都做不了）。2026-10-19 撤除。原始内容从来不需要它——channel 日志（ns 33）已经收了每一条消息。
- **初始化（人设 / 风格）**——图派生：从用户节点出发遍历组装，投到
  投影头部。尚未分配 ns。

## 事实图新增（2026-10-19）

- `EntityRow` 增加 `community_id`（声明热字段，`#[ok_default(0)]`；payload 布局 v2）
  及其 `by_community` 面（`[community_id][scope][node_id]`）——桶读是一次面扫描，
  不是全表过滤（ADR-0010 §2）。
- `FactEdge` 携带权重系统的输入（`read_count`、`write_count`、`tool_count`、
  `last_read`、`last_write`；布局 v2）。`weight` 是折出来的毫单位公式值。原声明的
  三个面（`by_since` / `by_confidence` / `by_weight`）**不再声明**：没有读者扫它们，
  而每次 touch 都要为不存在的读者维护（ADR-0005）。
- `CommunitySeqRow`（ns 38）：`[scope]` → `high_water`，最后发出的社区 id。
  每 scope 一行，原地覆写，单调递增——取代了 `HighWater` reduce 与「读现存社区
  最大值」两种发号（ADR-0010 §1）。ns 38 曾经装的那张胶囊表已退役。

## Flat memory（ns 32）

**2026-10-19 撤除**——图落地，载体随之退场（ADR-0001 的「flat memory, then the graph」；PLAN Phase 3）：表、`MemoryStore` 与 `/memory` 门都已删除，ns 32 空出。下面的 schema 作为图谱化之前形态的历史记录保留——存量行是迁移问题（ADR-0002 的 PG 导入），不是活表。

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

### Row：`MemoryRow`（`DocumentEncode`）

```rust
#[derive(DocumentEncode, ...)]
#[ok_ref(MemoryKey)]
#[ok_ns(32)] // row 是表的声明点
pub struct MemoryRow {
    pub text: String,           // cold TLV 段
    pub created_at: u64,        // hot 段（定宽）
    pub embedding: Vector<f32>, // cold 帧，长度任意——该行的向量
}
```

- ns 32 声明在 row 上（`#[ok_ns]`）；key 类型不携带 ns（collection 不收
  ns：组装用 `<MemoryRow as Document>::collection(store>`）。
- Payload：定宽字段连续排列（hot），变长 text 走 TLV（cold）。布局
  版本默认 1。`embedding` 追加在 cold 段尾部——新字段一律追加在尾部
  （okm 的 schema 演进规则）。
- `embedding` 是内置引擎的输出（ADR-0003），在源头做 L2 归一，于是契约
  里的 `cosine` 就是内积。帧自带计数，所以换模型换的是**数据**、不是
  wire；空帧表示该行写入时没有挂引擎（合法，检索侧跳过它——它不是
  零向量）。
- 尚无 `#[ok_index]` 访问方法——检索是 Phase 0 的暴力 substring 扫描
  （`scan_keys` + 过滤）。这是声明的 bring-up 姿态（ADR-0003），非终
  态；Phase 1 检索内置本地 embedding 模型的向量（`Vector<f32>` 帧，
  读进内存表）+ 名/别名点查 + weight 元数据（ADR-0005）。词法分支
  （jieba / 倒排索引 / BM25 / RRF / arroy）已撤销——ADR-0003。

### 身份生成

`id` 是 `MemoryStore` 分配的自增代理键。计数器不持久化：`open()` 扫
描表序最后一个 key 恢复（`[user_id][id]` BE → 末位 key 携带全局最大
id），从其之后继续。后果：id 唯一但不按 user 连续，计数器跨 user 全
局——不要假设 per-user 的 id 密度。

## 访问方法清单

okm 的纪律：查询只走声明过的路径——没有通用全表扫描的查询面。本仓的清单按形态分
三类，每条都写明它是哪一类：

- **点读**：`get`（主键）。
- **具名面**：声明的访问方法（前缀扫 / 区间扫 / 覆盖扫；等值匹配用 `scan_exact`）。
- **声明的退化路径**：全表扫描或表内过滤。它合法，但必须在清单里登记形态、前提与
  上限；前提失效（规模过界或成为热路径）时提升为具名面——声明即注册，加一条索引，
  而不是改调用点。

现状：

- Flat memory（ns 32）：**声明的退化路径**——`[user_id]` 前缀扫描加表内 substring
  过滤（ADR-0003 的 bring-up 姿态）。这张表**曾**是长期记忆的图前载体，已随图
  落地撤除（ADR-0001；2026-10-19），所以 ADR-0003 描述的向量落址与名/别名
  点查声明在图的两个 collection 上，不在这张表上。
- Channel 消息（ns 33）：**具名面**——`[channel_id][ts_ms][sub]` 前缀扫描：回放、
  increment、max-gap 读都是一次前缀扫描，且区间就落在主表自己的排序段上（顺序扫、
  无回表），见 ADR-0008 §3。checkpoint（ns 34）：按 `[channel_id][member_id]` 点读。
- Code（ns 35）：`get(code_id)` 是**点读**；`list()` 是**声明的退化路径**（全表按
  id 序，供解析与关联挑选，由图谱检索取代——ADR-0003/0018）。
- Fact graph：OKM 节点 collection（entities）+ 边 collection（facts）带声明面，
  见 ADR-0010 §1——不是 `edge:{src}:{label}:{dst}` 那种键布局（`EdgeTable` 是
  旧名：上面那层是 `Graph<S, E: GraphEncode>` + `NodeRef` + 谓词词汇表，okm
  ADR-0017）。weight 二级索引 = 边 collection 的 `weight` 声明字段的面
  （ADR-0005、ADR-0010 §1）。

## Hex 稳定性

Key 布局由各套件自己的 hex 测试锁定——`tests/channel.rs` 与 `tests/code.rs` 的
`key_layout_is_hex_locked`、`tests/graph.rs` 的
`entity_key_layout_is_hex_locked` 与 `edge_payload_layout_is_locked`；flat 记忆
那把锁随表一起撤除（2026-10-19）。不含 ns 头（`KeyEncode::encode()` 只返回
payload，不含 ns；Table 自行前置 `[ns 2B][slot 1B]`）。

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
coverage_ts, summary)`。`view::assemble(channels, graph, channel, member,
scope, user_node)` → `Projection { head: Vec<FactView>, view: SessionView }`
——头部是用户节点的双向关联事实（36/37），由门在出网前按请求的 `format`
渲染（`render::render_head`）。

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
    pub embedding: Vector<f32>,
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
- `name` 是**属性**（永不进键）；`kind_id` 走本集合自己的 **谓词词汇表**（固定
  面 0x04/0x05，okm ADR-0017 §3）——与它的 props 名所用的 0x02/0x03 词汇表分开，
  于是没有两个分配者共享同一个 id 空间。
- `literal_num` 是字面量节点的整数投影（其余为 0）：声明的、定宽的、有序的字
  段，于是将来的数值读仍然是对某个面做 range scan，而不是向量距离的猜测。
- **实体属性是合并的。** 一个词项的属性（元组的 head/tail 属性）落在节点行的
  **动态段**（slot 1）——那是这个实体的属性，被关于它的每条事实共享——而且是
  合并：同名属性取较新的值，其余存活。
- **`embedding` 是节点的向量（ADR-0003）**：内建引擎对实体**名字**的输出，在
  ingest 时于进程内计算——HTTP 客户端从不发送向量。任意长度的冷段帧（长度住在
  帧里，所以换模型换的是数据、不是 wire），源头做 L2 归一化，于是 `cosine` 就是
  一次内积。**空帧是合法行**（写入时没有挂引擎），检索侧跳过它，绝不是零向量。
- **检索（`GraphStore::retrieve`、`GET /graph/{scope}/search?q=&k=`——**分析**类：
  只有落点，供检视；核心答案是下面的 `recall`）是向量落地，
  不是查询语言**：嵌 query → 与该 scope 的**派生**内存表做内积 → 丢掉
  `LANDING_FLOOR`（0.35）以下的 → 取前 `k` 个种子 `{node_id, score, name,
  kind}`。空结果是合法的（什么都没落地，调用方不注入记忆）。表在首次使用时从
  KV 重建、**每次写入即失效**，所以不可能拿旧语料作答；它带字节预算与 LRU 淘汰，
  扫描形态是 `Flat`（暴力）——分桶形态是文档化的下一步，只在单个 scope 的表装不下
  时（ADR-0003 的可测阈值）才取。
- **`GraphStore::recall`（`GET /memories/{scope}/recall?q=&k=&budget=&ts_ms=`——**核心**
  类：消费方跑在上面的记忆答案**）是落地 + 着陆到的**社区**：种子的 `community_id`
  当作桶读（成员走 `by_community` 面，两端都在社区内的事实走遍历面），按权重
  （ADR-0005）排序，受 `budget` 条事实封顶，渲染成带 id 的文本——一个表头、每节点
  一行、每事实一行。空落地就是空答案（无种子、无文本），不是错误。更深的 walk 由
  调用方在 `out_edges` / `in_edges` 上组合。
- **`community_id` 与它的 `by_community` 面** 属于 ADR-0010 的步骤 3–4
  （增量 LPA + 分箱），并随之落地（2026-10-19）：读侧把这个面当作社区的桶来
  消费。ADR-0010 §1 保留设计形态。

### 事实（ns 37）——边集合

一个 okm 边集合（`Graph<FjallStore, FactEdge>`），每图一个 ns（okm ADR-0017）。
声明**就是**这张图——`GraphEncode`：`#[ok_ns(37)]`、`#[ok_nodes(FactNodes)]`
（成员集合——今天是 `Entities`；再加一个集合就是多一个枚举变体、`NS_VALUES` 里加一个 ns，改代码）、载荷的
编解码、以及每条**声明**的 `#[ok_index]` 一个 0x1 面；主表、载荷的动态段条目、
谓词词汇表与四个遍历面由句柄造。

| 面 | 键 |
|---|---|
| 0x0 主表 | `[edge_id 8B BE]` → `[src NodeRef][dst NodeRef][predicate_id][obj 载荷]` |
| 0x1 动态段 | `[edge_id 8B BE]` → 关系的开放 props（`functional` 标志住这里） |
| 0x1nnn 声明索引（`since` / `confidence` / `weight`） | `[字段值][edge_id]` |
| 0x4/0x5 谓词词汇表 | `[predicate_id]` → 谓词名 / `[谓词名]` → `predicate_id` |
| 0x6 谓词索引 | `[predicate_id][edge_id]` |
| 0x7 出边 | `[src NodeRef][edge_id]` |
| 0x8 入边 | `[dst NodeRef][edge_id]` |
| 0x9 谓词+出 | `[predicate_id][src NodeRef][edge_id]` |
| 0xA 谓词+入 | `[predicate_id][dst NodeRef][edge_id]` |

- **这一侧没有 scope 段。** 边集合自己造键（okm 的边形态），所以归属边界走
  **端点**：`NodeRef` 是 `[ns 2B][len varint][实体键]`，实体键里带着 scope。
  遍历从带 scope 的节点出发，也就一直在 scope 内；只有整 ns 的扫描（0x6，
  「某个谓词的全部边」）跨 scope，按端点过滤是调用方的事。要在边键上带
  per-scope 前缀，就得绕开 okm 的 `Graph` 手写这个集合。
- 三个声明字段是关系 props 的**投影**：`since`（调用方的时钟——裁决输入）、
  `confidence`（写侧的；255 = 缺省，即不惩罚）与 `weight`（权重系统的，
  ADR-0005——在该系统写它之前为 0）。它们被排除在边的动态条目之外：一个数据
  只有一个家。
- **关系的开放 props 走边的动态段**（边 ns 的 slot 1），用 okm 的
  `put_frame_named` 构造，与 link **同一批**提交（ADR-0002：一个 WriteBatch
  = 一个事务）。按断言给的 `functional` 标志留在那里——这是该断言自己关于
  「它如何声明那个种类的基数」的记录。
- **载荷编解码归声明所有（2026-10-19 落地）。** `FactEdge` 的载荷就是一份普通
  obj 载荷（`[version u8][hot_len u16 BE][since u64][confidence u8][weight u32]`），
  由同一份声明写与读：`encode_payload` / `decode_payload`。k10r 手写的
  `from_attrs` 与那把锁测试已删；裁决输入是
  `FactEdge::decode_payload(&body.payload)`。
- **词汇表纪律（一个 ns 两套词汇）——已退役，不再有替代品。** 边 ns 里曾住着关系
  种类（由 okm 的 `Graph` 用长驻 `VocabCache` 分配）**和** k10r 分配的 prop 名；
  两个 cache 在同一本词汇表里分配会把同一个 id 发给两个不同的名字，帧随后以错误的
  名字读回来，而且无声。okm ADR-0017 §3 把两套词汇分开（谓词占 0x04/0x05、载荷
  字段名保留 0x02/0x03）之后，这个撞名结构性消失；k10r 那两条纪律（一次性
  cache、handle 每次现建）随之退役，读侧的"预热"也不再需要（okm 的
  `VocabCache::id_get` 在探测时自加载）。实体集合走同一条规则——kinds 占
  0x04/0x05、props 名留 0x02/0x03。

### 访问面（mem-core `GraphStore`）

`ingest(scope, fact)` 解一条 8 元组并跑 ADR-0010 §1 的步骤 1–2：两个端点经
`entity_by_name` 解析（不存在则新建，并分配它的种类），随后关系要么挂一条新
边，要么——当事实声明了 `functional`——与该源上该种类的在册边裁决（`since >`
或 `confidence >=` 胜出：同一 `edge_id` 就地改写；否则这条事实被丢弃）。返回
`IngestOutcome::{Linked, Rewritten, Dropped}`。

读：`entity(scope, node_id)`、`entity_by_name(scope, name)` → `(node_id, row)`、
`entity_kind(kind_id)` → 词项、`entity_props` / `fact_props`（动态段）、
`fact(edge_id)` → 解出的事实、`out_edges` 与 `typed_out`（遍历通货）。批量消费者
（可视化门）用 `scope_entities(scope)` 与 `scope_facts(scope)` 整取一个 scope——
扫描以主键前缀 `[scope]` 为界，而每条边只被走到一次（`ingest` 把两个端点都建在
调用时给的 scope 里）。函数式改写是**一个**引擎批——okm 的 `unlink_into` +
`link_into`，加上 k10r 自己那条例子的动态段条目，边 id 不变，两次提交之间的窗口
不存在（ADR-0010，2026-10-19 付清）。

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
