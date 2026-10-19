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

| 门（HTTP） | mem-core 原语 | 表（ns） | 状态 |
|---|---|---|---|
| `POST /channels/{c}/messages` | `ChannelStore::append` | messages (33) | 已落 |
| `GET /channels/{c}/members/{m}/session` | `ChannelStore::fetch_full` | messages + checkpoints (33/34) | 已落 |
| `POST /channels/{c}/members/{m}/checkpoint` | `ChannelStore::append_checkpoint` | checkpoints (34) | 已落 |
| `POST /code` | `CodeStore::put` | code (35) | 已落 |
| `GET /code` | `CodeStore::list`（池读：id + language + description，无 source） | code (35) | 已落 |
| `GET /code/{id}` | `CodeStore::get`（整行，含 source） | code (35) | 已落 |
| ~~`POST /memory`~~ | ~~`MemoryStore::store`~~ | flat memory (32) | **2026-10-19 撤除**——载体是事实图（ADR-0001） |
| ~~`GET /memory/{user_id}/search?q=`~~ | ~~`MemoryStore::search`~~ | flat memory (32) | **2026-10-19 撤除** |
| ~~`MemoryStore::forget` / `list` / `stats`~~ | — | flat memory (32) | **2026-10-19 撤除** |
| `POST /facts/{scope}/edges/{id}/touch` | `GraphStore::touch`（调用方上报的工具调用：计数加一、权重重折；未知边或别的 scope 404） | facts (37) | 已落（2026-10-19） |
| `POST /facts/{scope}` | `GraphStore::ingest`（请求体就是 8 元组；`scope` 走路径） | 事实图——实体（36）+ 事实（37） | 已落（2026-10-17） |
| `GET /facts/{scope}/entities/{name}` | `GraphStore::entity_by_name`（名/别名精确入口；未知名字 404） | 实体（36） | 已落（2026-10-17） |
| `GET /facts/{scope}/entities/{node_id}/edges` | `GraphStore::out_facts`（一跳，两端都已解析） | 事实（37）+ 实体（36） | 已落（2026-10-17） |
| `GET /graph/{scope}` | `GraphStore::scope_entities` + `scope_facts`（一个 scope 作为节点 + 连边——渲染方的批量读） | 实体（36）+ 事实（37） | 已落（2026-10-17） |
| `GET /facts/{scope}/search?q=&k=` | `GraphStore::retrieve`（落点：节点向量、内积、0.35 地板；无 engine = 200 + `[]`） | 实体（36） | 已落（2026-10-19） |
| `GET /facts/{scope}/recall?q=&k=&budget=&ts_ms=` | `GraphStore::recall`（落点 → 着陆到的**社区**的桶 → 渲染好的带 id 文本；事实按权重排序、受 `budget` 封顶；无 engine = 空答案） | 实体（36）+ 事实（37） | 已落（2026-10-20） |
| — 无路由 | `entity` / `entity_props` / `fact` / `fact_props` / `typed_out`（核心其余读），以及 scope 级的权重 Top-K | 事实图（36/37） | core 已落、无路由；scope 级 Top-K 是后续件（Phase 4——需要 `[scope][weight][edge_id]` 投影表，ADR-0005） |
| — 无路由 | 初始化记忆装配（从用户节点出发遍历） | initialization memory | 已声明（ADR-0001），无 ns |
| — 无路由 | 任务顶点 + `task_links` 边（第三个接口面——一个图，与事实同图） | task graph | 已声明（ADR-0001），无 ns |
| 读侧随会话投影；写侧无路由 | 任务实例态（开 / 完——用于精确枚举的结构化字段） | task state | 已声明（gravity ADR-0003 §3/§4） |

in-process 不是第二道门：`InProcessSurface` 在调用方进程内链接同一批原
语（同机部署细节）；HTTP 是唯一的跨进程形态（ADR-0007 as revised）。

## 2. gravity 的操作 → 各自走的门

| gravity 操作 | gravity 侧名称 | 门 | mem-core 原语 | 状态 |
|---|---|---|---|---|
| 轮首取投影 | `MemorySurface::fetch_session` | `GET /channels/{c}/members/{m}/session` | `ChannelStore::fetch_full` | 已落 |
| 轮末追加 delta（用户消息 + 回复，同一个 `ts_ms`，`sub` 0/1） | `MemorySurface::append_message` | `POST /channels/{c}/messages` | `ChannelStore::append` | 已落 |
| 压缩落地（summarize） | 未接 | `POST /channels/{c}/members/{m}/checkpoint` | `ChannelStore::append_checkpoint` | 门已落，gravity 侧欠（Phase 2/2.6） |
| 任务进度（一步一 task 行；读随轮首投影，完成在执行点写） | ADR-0003 §3/§4 | 读侧随 `GET /channels/{c}/members/{m}/session`；写侧尚无路由 | 任务顶点 + `task_links` 边，与事实同图 | 已声明——第三个接口面（ADR-0001）；gravity Phase 2.6 |
| `recall_memory`——长期事实 | 工具面（未进代码） | `GET /facts/{scope}/recall?q=`（落点 + 着陆到的社区，已渲染）与 `GET /facts/{scope}/entities/{name}`（精确入口） | `GraphStore::recall` / `entity_by_name` | 图的门已落（2026-10-20）；工具面是后续件（gravity 侧） |
| `commit_memory`——写事实 | 工具面 | `POST /facts/{scope}` | `GraphStore::ingest(fact)` | 门已落（2026-10-17）；工具本身是后续件（gravity 侧） |
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
  修改。日志不属此列：它保持只追加，压缩点住在自己的表里、用 `coverage_ts`
  指向日志。
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

- 新门在**落地同一批**登记在此（路由 + 原语 + ns + 状态），通常与它的
  PLAN 子步、ADR 正文一起。
- 门的状态只能凭证据移动：走真实服务的集成测试，或锁住原语的核心测试。
- 设计问题不住在这里。一句话若需要论证，它属于 ADR，本页只指过去。