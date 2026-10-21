# SEAM — 消费者操作与 k10r 接口的对应

> **语言：** [English](SEAM.md)（主文档）· [中文](SEAM.zh-CN.md)

这页是什么：**清单**。哪个门被敲、那个门在 k10r 内部对应什么、门是否已
经存在。每个门背后的设计在各 ADR（`docs/adr/`）；schema 在
`docs/MODELING.md`；本页只陈述对应关系，好让任何一侧的改动都有一个地方
可看。

今天只有一个消费者：**gravity**（turn executor）。它的操作列在下面；第
二个消费者（记忆主导的程序、没有 turn 循环的 agent）的加入方式是加一
节，不是加一套机制。

## 1. k10r 的门（已实现的形态）

路由声明在 `crates/mem-server/src/lib.rs`（router 块）；原语在
`crates/mem-core/src/`。

**门分两类，不可互换。**

- **核心 API**——消费方的操作真正跑在它上面：循环的上下文读与追加、记忆答案、
  身份精确入口、调用方上报的计数、code 存储。这一类的**形状就是契约**：改动是
  ADR 级别的决定；消费方的一轮只能走这一类。
- **分析 API**——只读的检视面，给审计、可视化、调试用：原始图、单独一步落点、
  一跳、无路由的核心读原语。它**次一等**：形状可以随分析需要改，永远不进模型的
  工具清单，消费方的一轮里任何东西都不许依赖它。

这条线存在的理由：内部结构可以**出现在核心结果里**当句柄（node id、edge id、
community id），但**维护**那个结构从来不是消费方的活儿。所以「请消费方去列出、
拉取、修补、回写 k10r 自己的索引」的门既不属于核心也不属于分析——它是设计错误；
曾经待在这里的那四扇固化门就是它（2026-10-20 撤除）。

### 1a. 核心 API——`/channels`、`/memories`、`/code`

| 门（HTTP） | mem-core 原语 | 表（ns） | 状态 |
|---|---|---|---|
| `POST /channels/{c}/messages` | `ChannelStore::append` | messages (33) | 已落 |
| `GET /channels/{c}/messages` | `ChannelStore::history`——**人的视图**（ADR-0009 §4）：按日志序给每一条，带归属名，没有 checkpoint、没有摘要、没有合并 | messages (33) + members (42) | 已落（2026-10-20） |
| `GET /channels/{c}/members/{m}/session?format=&ts_ms=` | `channel::fetch_full` + `view::assemble`（头部 → 压缩点 → 增量）+ `render::render_head`；读取时跑 `compress::evaluate`，判词随 `compress: {checkpoint, prompt}` 出行——引擎先在那个切分点上开出「正在压」的节点（空摘要 = 标记，DTO 的 `compressing` 据此为真）；`note_read` 盖的是**这个成员**的游标（缓存钟量它；单人+agent 没有行时就是日志最后一条） | messages (33) + checkpoints (34) + cursors (39，多方) + 头部的那些事实 (36/37) | 已落（视图形状 2026-10-20；压缩判词 2026-10-20） |
| `POST /channels/{c}/members/{m}/checkpoint` | `ChannelStore::complete_checkpoint`（正文只有摘要；填进 k10r 开出的那一行；没有开着的行 = 400） | checkpoints (34) | 已落（2026-10-20） |
| `POST /memories/{scope}` | `GraphStore::ingest`（请求体就是 8 元组；`scope` 走路径） | 实体（36）+ 事实（37） | 已落（2026-10-17） |
| `GET /memories/{scope}/recall?q=&k=&budget=&ts_ms=&format=` | `GraphStore::recall`（落点 → 着陆到的**社区**的桶 → 一种表示：渲染好的带 id 文本，或 `format=struct` 时只给数据；按权重排序、受 `budget` 封顶；无 engine = 空答案） | 实体（36）+ 事实（37） | 已落（2026-10-20） |
| `GET /memories/{scope}/entities/{name}` | `GraphStore::entity_by_name`（名/别名精确入口；未知名字 404） | 实体（36） | 已落（2026-10-17） |
| `POST /memories/{scope}/edges/{id}/touch` | `GraphStore::touch`（调用方上报的工具调用：计数加一、权重重折；未知边或别的 scope 404） | facts (37) | 已落（2026-10-19） |
| `POST /code` | `CodeStore::put` | code (35) | 已落 |
| `GET /code` | `CodeStore::list`（池读：id + language + description，无 source） | code (35) | 已落 |
| `GET /code/{id}` | `CodeStore::get`（整行，含 source） | code (35) | 已落 |

已从这一类撤除（2026-10-19，随 flat 记忆）：`POST /memory`、
`GET /memory/{user_id}/search?q=`、以及 `MemoryStore::forget` / `list` /
`stats`——载体是事实图（ADR-0001）。

已声明、尚无门（核心侧，后续阶段）：初始化记忆装配（随会话投影的头部到达，
ADR-0001/0008）、任务顶点 + `task_links` 边与任务实例态（第三个接口面，
ADR-0001；gravity ADR-0003 §3/§4）。

**内部诊断没有门。** 结构化日志（ns 41，ADR-0011）由 k10r 自己写，只能通过将来
analysis 类的门读；它永不在消费者的回合路径上，也永不作为工具出现。

### 1b. 分析 API——`/graph`

| 门（HTTP） | mem-core 原语 | 表（ns） | 状态 |
|---|---|---|---|
| `GET /graph/{scope}` | `GraphStore::scope_entities` + `scope_facts`（一个 scope 作为节点 + 连边——渲染方的批量读；节点带 `community_id`） | 实体（36）+ 事实（37） | 已落（2026-10-17） |
| `GET /graph/{scope}/search?q=&k=` | `GraphStore::retrieve`（**只有落点**：节点向量、内积、0.35 地板；无 engine = 200 + `[]`）——`recall` 回答之前做的那一步 | 实体（36） | 已落（2026-10-19） |
| `GET /graph/{scope}/entities/{node_id}/edges` | `GraphStore::out_facts`（一跳，两端都已解析）——检视用；调用方要自己组更深的走法也从这里起步 | 事实（37）+ 实体（36） | 已落（2026-10-17） |
| `GET /graph/{scope}/entities/{node_id}/anchored` | `GraphStore::anchored`——实体锚定入口（PLAN 4b）：先给碰这个节点的边，再接它的 `community_id` 切片，权重降序；未知节点 404 | 实体（36）+ 事实（37） | 已落（2026-10-20） |
| `GET /graph/{scope}/timeline?k=` | `GraphStore::timeline`——时间入口：该 scope 的事实按 `since` 排，最新在前，走 `by_since` 面（该面不带 scope：别的 scope 的键跳过） | 事实（37） | 已落（2026-10-20） |
| `GET /graph/{scope}/chain?from=&kind=&hops=` | `GraphStore::chain`——因果入口：沿一个具名谓词逐跳，防环、有界；每行的 `depth` 就是行走的顺序 | 事实（37）+ 实体（36） | 已落（2026-10-20） |
| `GET /graph/{scope}/facts/{edge_id}` | `GraphStore::fact_in_scope`——**确定性**读：写返回的那个 `edge_id` 直接读回来，没有查询、没有向量；scope 与边自身的端点核对，所以别的 scope 的 id 是 404（这正是参考链接的地址） | 事实（37） | 已落（2026-10-20） |
| `DELETE /graph/{scope}/facts/{edge_id}` | `GraphStore::withdraw`（okm 的 `unlink`：行与全部派生面条目在**一个**批里删净；204，或 id 在本 scope 不存在时 404） | 事实（37） | 已落（2026-10-20） |
| — 无路由 | `entity` / `entity_props` / `fact` / `fact_props` / `typed_out`（核心其余**读**原语） | 事实图（36/37） | core 已落、无路由——审计读 |
| `GET /graph/{scope}/top?k=` | `GraphStore::top_by_weight`（该 scope 最重的事实，权重降序、同权重按 edge id；排序来自声明过的 `by_weight` 面——ADR-0005 的 `W:{weight}:{edge_id}`，每次写都维护——**结果**由 `k` 截断） | 事实（37） | 已落（2026-10-20；走 `by_scope_weight` 面：O(k 行) + O(scope 键)；惰性键扫描仍欠在 okm-core） |

in-process 不是第二道门：`InProcessSurface` 在调用方进程内链接同一批原
语（同机部署细节）；HTTP 是唯一的跨进程形态（ADR-0007 as revised）。

## 2. gravity 的操作 → 各自走的门

这张表里的每个操作都走**核心**门（§1a）；分析类（§1b）在消费方的一轮里
一个都不出现。

| gravity 操作 | gravity 侧名称 | 门 | mem-core 原语 | 状态 |
|---|---|---|---|---|
| 轮首取投影（头部是 k10r 渲染的文本，格式由 gravity 配置给出；`struct` 被拒） | `MemorySurface::fetch_session` | `GET /channels/{c}/members/{m}/session?format=` | `ChannelStore::fetch_full` + `render::render_head` | 已落；签名不随指针裁决变动——读取是**那个成员**发出的动作（盖它的游标，答案也是它的投影） |
| 轮末追加 delta（用户消息 + 回复，同一个 `ts_ms`，`sub` 0/1） | `MemorySurface::append_message` | `POST /channels/{c}/messages` | `ChannelStore::append` | 已落 |
| 压缩落地（summarize） | 未接 | `POST /channels/{c}/members/{m}/checkpoint` | `ChannelStore::complete_checkpoint` | 门已落，gravity 侧欠（Phase 2/2.6） |
| 任务进度（一步一 task 行；读随轮首投影，完成在执行点写） | ADR-0003 §3/§4 | 读侧随 `GET /channels/{c}/members/{m}/session`；写侧尚无路由 | 任务顶点 + `task_links` 边，与事实同图 | 已声明——第三个接口面（ADR-0001）；gravity Phase 2.6 |
| `recall_memory`——长期事实 | 工具面（未进代码） | `GET /memories/{scope}/recall?q=`（落点 + 着陆到的社区，已渲染）与 `GET /memories/{scope}/entities/{name}`（精确入口） | `GraphStore::recall` / `entity_by_name` | 图的门已落（2026-10-20）；工具面是后续件（gravity 侧） |
| `commit_memory`——写事实 | 工具面 | `POST /memories/{scope}` | `GraphStore::ingest(fact)` | 门已落（2026-10-17）；工具本身是后续件（gravity 侧） |
| `resolve_skills`——可选 skills 列表 | 工具面 | `GET /code`（bring-up：池读） | `CodeStore::list` | bring-up 形态已落；skill 节点（前置 / 步骤 / 经验 / 后验）是 Phase 5+ deferred |
| `invoke_skill`——计划里取码那一步 | 工具面 | `GET /code/{id}` | `CodeStore::get` | 已落 |
| `invoke_skill`——调用本身 | 计划里的一步 | **不走 k10r**（见 §3） | — | gravity Phase 4.6 |
| 初始化记忆（人设 / 风格） | 调用方无操作：它随投影到达 | — | 装配是 k10r 内部细节（从用户节点出发遍历，注入上下文头部） | 已声明（ADR-0001） |

## 3. 不属于 k10r 门的部分

执行从不落到 k10r。`invoke_skill` 通过框架里受管控的调用派发——
`invoke(mount_type, session_key, "execute", {"code": <source>, "args":
<model args>})`——到 **effector 节点 mount**；脚本是在上一步从 k10r 取出来
的（`GET /code/{id}`），作为调用参数随行。mount 注册面在 prism
（`crates/prism/src/mounts.rs`）。所以两个族在边缘处分道：记忆族（以及技
能族的**解析**）是打到 k10r 的调用，技能族的**执行**是打到节点的调用。
k10r 的任何路由都不会成为 LLM 的工具目标，k10r 也从不回调——它不持有策
略，也不是 actor（ADR-0007）。

## 4. 让这张表读得通的几条不变量

- **追加优先是**图**的规则；会话日志只追加**（ADR-0002）：凡
  承载知识或数据的表（节点/边行、权重、派生项、code 行）默认追加——无读改
  写竞态、可重放、坏派生只是一次重试——并在「禁令比更新本身更贵」的地方允许
  修改。日志不属此列：它保持只追加，压缩点住在自己的表里，是指向某一条记录的
  `(ts_ms, sub)` 指针。
- **调用方持时钟。** `ts_ms` / `sub` 随请求走；服务端从不分配
  （ADR-0008 §3）。任何新门都带同一条规则。
- **一库两 keyspace，按写形态分**（每进程一个、一把锁）：`channel`
  （ns 33/34——队列式，自带 LSM 树，压缩不拖别的表）与 `memory`（今天
  35/36/37/38，之后是技能；32 是已撤的 flat 记忆）。新表加入它写形态所属的那个 keyspace；批不跨
  keyspace。
- **gravity 自己讲 mem-server 的 wire，而且是两处**：`HttpSurface`
  （`crates/core/src/surface.rs`）与 booth 的 turn guest
  （`crates/booth/src/bin/turn.rs`，自己拼同样的 URL）——一份契约、两个调
  用点，靠手工保持一致。
- **计数归 k10r，动作归 gravity**：`tool_invoke_count` 走权重系统
  （ADR-0005，Phase 5），而每个工具的快/幂等 vs 触人/外部性质是在 gravity
  侧注册时声明的（gravity Phase 2.5）。
- **失败是值**，不是撕裂的写：失败的一轮什么都不追加；每道门要么以一次
  完整的交流被调用，要么完全不动。

## 5. 保持这页为真

- 新门在**落地同一批**登记在此（路由 + 原语 + ns + 状态，**以及它的类别**——
  核心 §1a 还是分析 §1b），通常与它的 PLAN 子步、ADR 正文一起。类别说不清的
  门，就是还没到能落地的时候。
- 门的状态只能凭证据移动：走真实服务的集成测试，或锁住原语的核心测试。
- 设计问题不住在这里。一句话若需要论证，它属于 ADR，本页只指过去。