# Krystallizer 数据建模

memory core 的 schema 集中一处：现有哪些表、key 与 row 形态、声明了
哪些访问方法。OKM 通用建模规则（四层、访问方法纪律、键尾规则）不在
此重复——见 okm 的
[MODELING.zh-CN.md](https://github.com/orbsh/okm/blob/main/docs/MODELING.zh-CN.md)，同样对本仓
规范生效。每张表背后的设计决策见各 ADR。

> **Languages:** [English](MODELING.md) · [中文](MODELING.zh-CN.md)（主文档）

## 表

一个实体类 = 一个 ns（OKM 规则）。现状：

| ns  | 实体              | 状态                     | ADR      |
|-----|-------------------|--------------------------|----------|
| 33  | Channel 消息日志   | 已实现（最小切片，2026-10-09） | ADR-0008 |
| 34  | Checkpoint（channel 的指针 → 摘要 + 人设快照）| 已实现（最小切片，2026-10-09） | ADR-0008 |
| 35  | Code（交付的脚本） | 已实现（bring-up 切片，2026-10-15） | gravity ADR-0001/0003 |
| —   | Tasks / task_links DAG | 已声明（ADR-0001——第三个接口面；与事实同图） | ADR-0001, ADR-0002 |
| 36  | 实体（事实图）     | 已实现（2026-10-17）      | ADR-0010 |
| 37  | 事实（事实图边）   | 已实现（2026-10-17）      | ADR-0010 |
| 38  | 社区序号（发号用，每 scope 一行） | 已实现（2026-10-20） | ADR-0010 |
| 39  | 读游标（channel 的指针；仅多方） | 已实现（2026-10-20） | ADR-0008 §2 |
| 41  | 结构化日志（诊断用；内部，无门） | 已声明 | ADR-0011 |
| —   | 初始化记忆（人设 / 风格） | 已声明（ADR-0001） | ADR-0001 |

图的读侧（LPA、分箱、社区的渲染）已随之落地（2026-10-19/20）；会话日志缺的几块、
任务、以及 scope 级 Top-K 需要的 `[scope][weight][edge_id]` 投影表，属后续阶段；
上表标 `已声明` 的行是 ADR 承诺，非运行代码。

## 记忆种类（谁发起）

三种，按谁驱动分（ADR-0001）；命名是 被动 / 主动，
短期 / 长期 作内容时程的定语：

- **被动（短期）**——当前上下文：channel 日志（ns 33）+ 它的 checkpoint
  （ns 34：指向日志的指针 + 那一段的摘要）。循环自己取、自己追加，从不作为工具。
- **主动（长期）**——不在上下文里的事实，以**工具**形式到达，任何 agent 都能
  用。载体是事实图（ns 36/37）。原始对话不归这一类：channel 日志（ns 33）已经
  收了每一条消息。
- **初始化（人设 / 风格）**——图派生：从用户节点出发遍历组装，投到
  投影头部。尚未分配 ns。

## 访问方法清单

okm 的纪律：查询只走声明过的路径——没有通用全表扫描的查询面。本仓的清单按形态分
三类，每条都写明它是哪一类：

- **点读**：`get`（主键）。
- **具名面**：声明的访问方法（前缀扫 / 区间扫 / 覆盖扫；等值匹配用 `scan_exact`）。
- **声明的退化路径**：全表扫描或表内过滤。它合法，但必须在清单里登记形态、前提与
  上限；前提失效（规模过界或成为热路径）时提升为具名面——声明即注册，加一条索引，
  而不是改调用点。

现状：

- Channel 消息（ns 33）：**具名面**——`[channel_id][ts_ms][sub]` 前缀扫描：回放、
  increment、max-gap 读都是一次前缀扫描，且区间就落在主表自己的排序段上（顺序扫、
  无回表），见 ADR-0008 §3。checkpoint（ns 34）按**键序读**——`[channel_id][ts_ms][sub]`，
  最新 = 一次前缀区间扫描的最后一个键，所以「最近一次压缩」不需要排序；ns 40
  （已撤除的决策日志）空出。
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
`entity_key_layout_is_hex_locked` 与 `edge_payload_layout_is_locked`。不含 ns 头（`KeyEncode::encode()` 只返回
payload，不含 ns；Table 自行前置 `[ns 2B][slot 1B]`）。

## Channel 日志（ns 33）

一张只追加的消息表，加三张投影与压缩引擎读的表（ADR-0008）。声明在
`crates/mem-core/src/channel.rs`。

**没有成员维度，也没有第三张表。** 这里的两张表属于 **channel**，不属于某个读者：
checkpoint（channel 的指针 + 那一段的摘要 + 人设快照）与——只在**多方**形态才有的
——读游标。一份日志上不存在第二个压缩主体：agent 的边界随长期记忆流动，一条
channel 也只有一道声音——所以成员段只能是一段恒量，而恒量不是维度（ADR-0008 §2
及其 Why Not）。**消息也没有成员**：消息属于日志。成员是**发送者**（显示名 /
角色，Phase 2.7）——以**行字段**加序列化归属的形式进来，不进这里的键。原先立在
旁边的决策日志已撤除（2026-10-20）：「哪一段被压过」checkpoint 行自己就答了，而
它存的参数快照没有读者（没有标签就没有拟合）。`DEFAULT_MEMBER = 1` 是代码仍带着、
正在去掉的占位。

### `MessageKey` / `MessageRow`（ns 33）

```rust
#[derive(KeyEncode, ...)]
#[ok_ns(33)]
pub struct MessageKey {
    pub channel_id: u64,
    pub ts_ms: u64,
    pub sub: u16,
}
```

- 键 `[channel_id 8B][ts_ms 8B][sub 2B]`，BE 定宽，所以字节序**就是**
  `(ts_ms, sub)` 序：回放、增量、max-gap 读都是一次前缀扫描，落在日志自己的排序段
  上——不排序，也没有二级索引。
- `ts_ms` 是**调用方**的钟（ADR-0008 §3）；`sub` 破同毫秒平手（单写者、顺序追加）。
- 行：`role u8`（user/assistant/tool/system）、`kind u8`（text；tool_call/tool_result
  预留给视图层剪枝）、`content String`。存储永远持有完整行，剪枝归视图（ADR-0006）。

### `CheckpointKey` / `CheckpointRow`（ns 34）——channel 的指针

- 键 `[channel_id 8B][ts_ms 8B][sub 2B]`——**键就是指针**：一次压缩一行，指向它的
  摘要覆盖到的那条日志记录。读投影取**键序最大的、带摘要的那一行**——字节序就是
  `(ts, sub)` 序，所以「最近一次压缩」是一次键比较，不用排序。
- 行：`summary String`（**空 = 正在压**；引擎看到它就下一轮放过）与
  `prompt_snapshot String`（那次压缩时的人设/初始提示词文本——存快照而不是每次
  重算，前缀才字节稳定，prompt cache 才留得住）。
- 行是**累积**的，不覆盖：同一段压两次各写一行（读出来一样，代价一次多余的模型
  调用）；更早指针的摘要晚回来时落在最大行之下——读路径看不见，也不必删，那是
  免费的历史。

### `CursorKey` / `CursorRow`（ns 39）

- 键 `[channel_id 8B][member_id 8B]`——某个参与者的**读时钟**，在投影读取时盖上：
  缓存钟唯一的可观测物。只有**多方**形态才有这一行；单人与 agent 时不存，空闲时长
  直接是「现在 − 最后一条的 ts」。latest-wins，更早的读取不改动它——钟永不倒退。
  不存**位置**：每次读都答到最新一条，切增量的是 checkpoint。
- 行：只有 `read_at u64`——那次读取时调用方的钟。模型的每次读都答到最新一条，存位置
  就是个常数；缓存钟要的是「这个成员上次碰模型是什么时候」。

### 访问面（`ChannelStore`）

| 调用 | 做什么 |
|---|---|
| `append(channel, ts_ms, sub, role, kind, content)` | 追加一条，返回它的键 |
| `fetch_full(channel, member)` | `SessionView { coverage, summary, head, compressing, increment, last_read }`——摘要、头部、截断点都取**最近那条带摘要**的节点；它上面若有空摘要节点，只让 `compressing` 为真 |
| `open_checkpoint(channel, pointer, head)` | 在切分点上放下**空摘要**的节点（正在压的标记），带着冻结的头部 edge id；读仍由它下面那个节点作答 |
| `complete_checkpoint(channel, summary)` | 把摘要填进最新的未完成节点——门唯一的 checkpoint 写 |
| `append_checkpoint(channel, pointer, summary, head)` | 裸的一次性写（测试、回填）——门不调它 |
| `channel::member_node(channel, member)` | 成员事实所挂的图节点名——`member-{channel}-{member}`，是规则不是表（改名不动任何事实） |
| `note_read(channel, member, read_at)` / `read_at` | 盖 / 读读时钟（游标行是多方形才有的；不存位置） |

`view::assemble(channels, graph, channel, member, scope)` 返回
`Projection { head, view }`——头部是用户节点的双向关联事实（36/37），门在出网前按
`?format=` 渲染它。

尚未落地：thread、topic tag、members 表（ADR-0008/0009）——游标的「人」那一维
正是随这张表才回来。

代码已跟上模型（2.6d-i/ii 于 2026-10-20 落地）：键即指针、节点累积、
`open_checkpoint` / `complete_checkpoint` 就是那两个写、ns 40 已撤、头部是冻结的
edge id 集合。还欠的是游标的第二段（2.6d-iii：`ts_ms + sub`，且只有多方形才存行）。

## 结构化日志（ns 41）——诊断用，没有门

ADR-0011。表与 store 已在 `crates/mem-core/src/log.rs`；**还没有人写行**（压缩判词是
第一个写者，它等指针形状改完）。

### `LogKey` / `LogRow`（ns 41）

```rust
pub struct LogKey { pub ts_ms: u64, pub seq: u64 }
pub struct LogRow { pub level: u8, pub event: String }   // 另有动态段
```

- 键 `[ts_ms 8B][seq 8B]`——BE 定宽，字节序即时间序：「最近 N 条」「T 附近的窗口」
  与回放都是前缀/区间扫。`ts_ms` 是 **k10r 自己的钟**（写者就是进程；ADR-0008 §3
  的「调用方持钟」管的是调用方数据，不是进程自己的观察）。`seq` 是进程内单调计数，
  开库时从最后一个键续上（与 id 恢复同一套做法），所以重启是接着写而不是覆盖。
- 行：`level u8`（trace/debug/info/warn/error）与 `event String`，两个都取自常量
  词汇表，都是声明字段，且**都不挂 0x1 面**——面是每行多一条索引写，换来一个当前
  没人发的查询；等某个查询成了热路径再加。
- 变的部分全在槽 1 的动态段：**定位**（`channel` / `scope` / `entity` / `edge` /
  `msg`，哪个适用记哪个）与**载荷**（参数快照、信号实测值、切分点、错误原文）。
  嵌套值走 `put_frame_named`。
- 由 `logs { max-rows }` 封顶：写一行时在同一 batch 里从最老的键开始删。裁剪本身
  不记日志（否则裁剪产生日志、日志触发裁剪）。

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
`code_id`（代理键，重启后扫末键恢复）；
`get(code_id)` → 该行，未知 id = 不存在；`list()` →
`(code_id, language, description)` 按 id 序——解析方或关联方挑选的依据，
也是声明的 bring-up 姿态（全表扫描，由图谱检索取代，ADR-0003/0018）。
`list()` 不带 source 与参数定义。

## 事实图（ns 36 实体，ns 37 事实）

主动面（长期）的载体：一个节点集合 + 一个边集合，以及把一条事实落地的写时消解
（ADR-0010 §1）。声明在 `crates/mem-core/src/graph.rs`。

**`scope` 是什么。** 一个用户的记忆语料——下表的每一张图都把它放在键的第一段
（`u64` 用户身份；单用户部署里就是 `1`，即 `view::DEFAULT_SCOPE`）。它是检索与
遍历的边界：一个 scope 的语料装进一个 scope 的索引，bucket 读也只在一个 scope 内。
channel 侧没有 scope——一个 channel 本身就是某个 scope 的一段对话。

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
    pub embedding: Vector<f32>, // 没有 #[ok_index]：向量不按键序，见下
}
```

**向量不声明面。** `#[ok_index]` 声明的是「按键序做前缀 / 区间 / 覆盖扫」的派生条目；
向量不按键排序，所以没有它的面可声明。向量检索走 `okm-vector` 的 `ExactIndex`——
首次使用时从各行的 `embedding` 帧重建的派生索引（写入即失效），不是一张声明面。

- 键 `[scope 8B BE][node_id 8B BE]`——`scope` 打头是因为语料按用户划分
  （ADR-0003）。id 是 store 发放的代理键，重开
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
  不是查询语言**：嵌 query → 向该 scope 的**派生**索引要最近的几行 → 丢掉
  `LANDING_FLOOR`（0.35 余弦）以下的 → 取前 `k` 个种子 `{node_id, score, name,
  kind}`。空结果是合法的（什么都没落地，调用方不注入记忆）。**扫描是
  `okm-vector` 的，不是 k10r 的**：索引是它的 `ExactIndex`（一整块 f32 加键表），
  首次使用时从 KV 重建、**每次写入即失效**，所以不可能拿旧语料作答；字节预算与
  LRU 淘汰留在这边，因为那是 k10r 的内存账，不是搜索本身。索引是精确还是近似，是 `okm-vector`
  自己的事——该形态那个 crate 本来就有——不是 k10r 的 phase、也没有阈值参数，
  这边绝不自己养一份图实现（ADR-0003）。
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

一条事实 = 两个节点 + 一条边；这一张是**边**（ADR-0010 §1）。声明：

```rust
#[derive(GraphEncode, ...)]
#[ok_ns(37)]
#[ok_nodes(FactNodes)]           // 成员节点集合：今天只有 Entities
#[ok_index(
    by_since { fields(since) },          // 三个声明面，各一个 0x1 条目
    by_confidence { fields(confidence) },
    by_weight { fields(weight) },
)]
pub struct FactEdge { /* since / confidence / weight / 五个计数器 */ }
```

- **边集合自己造键，所以这一侧没有 scope 段。** 归属边界走**端点**：端点是
  `(ns, 主键)` 的引用，实体主键里带着 scope。遍历从带 scope 的节点出发，就一直在
  scope 内；只有整表扫描（「某个谓词的全部边」）跨 scope，按端点过滤是调用方的事。
  要在边键上带 per-scope 前缀，就得绕开库的边图手写这个集合——不值得。
- 声明字段是关系 props 的**投影**（一个数据只有一个家）：`since`（调用方的钟，裁决
  输入）、`confidence`（写侧的，255 = 缺省即不惩罚）、`weight`（权重系统的毫单位
  ），加权重系统折进去的五个计数器（见「事实」载荷字段清单）。
- **关系的开放 props 走边的动态段**，与 link 同一批提交（ADR-0002：一个 WriteBatch
  = 一个事务）。按断言给的 `functional` 标志留在那里——这是该断言自己关于「它如何
  声明那个种类的基数」的记录。
- 面号、词汇表、动态段的帧构造都是 **okm 的内部**，不在本页重复：见 okm 的
  [MODELING](https://github.com/orbsh/okm/blob/main/docs/MODELING.zh-CN.md)。本页只
  登记 k10r 自己声明的东西（ns、键字段、声明的面、载荷字段）和它依赖的那条结论。

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
树，它的 compaction 不会拖着别的表）与 `memory`（code + 事实图，将来
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
