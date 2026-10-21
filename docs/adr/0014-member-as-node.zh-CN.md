# 0014 — 成员就是节点：身份行加入图

状态：已接受（2026-10-21），同日落地

## 背景

ADR-0009 §1 把身份与画像分开：members 表（ns 42）持有确定性层（`member_id → 显示名 + 角色`），画像知识是图里的事实。这个分法站得住；站不住的是两层之间的**桥**。今天成员只能以一条实体行的形式进入图——行名是一条命名规则 `member-{channel}-{member}`（`channel::member_node`）——它像任何抽出来的词一样，由元组的类型槽建在实体 collection 里。三个后果，方向一致地错：

- **身份被当成了抽取出来的知识。** 成员代理行住在实体 collection 里，于是它带着 `embedding`、是检索的落点种子：一次查询可以落在一个人身上。检索的职责是落在知识上，而人不是知识——人是这个域的持有者，在抽取回路之外被管理。
- **代理行与身份层是两份真相。** `MemberRow` 已经有 `display_name` 和 `role`，代理行两样都没有，所以任何想要"用户节点的名字"的消费方都得回到 channel 侧去解析一个字符串。
- **图里没有用户端点。** "初始化记忆是从用户节点出发的一次遍历"（PLAN Phase 3）实际起点是摄入路径碰巧建出来的那行实体——它之为"用户"的唯一依据，是名字遵守了一条规则。

## 决策

### 1. members collection 就是图的用户节点集合

ns 42 进入事实图的成员集，方式是在 `#[ok_nodes]` 里点名它那张表（`#[ok_nodes(EntityRow, MemberRow)]`，成员集枚举与 ns 清单由 derive 生成，ns 取自各表自己的 `#[ok_ns]`）。别的什么都不需要——边的端点是自描述的 `[ns][len][pkey]`，所以成员行 `[channel_id][member_id]` 这个键本身就成了一条节点引用，不需要注册表、不需要第二张表、不需要名字。

这是**封闭类型**的形态，而且是刻意的：user 只有一种类型、声明一次，于是 collection 就是类型标记。实体保持开放 kind 的形态（kind 是一个 collection 内部带索引的字段，ADR-0010 §1），因为它们的 kind 在摄入时才到达。人不会"到达"——人是被登记的。

### 2. `member-{channel}-{member}` 变成**引用语法**，不再充当节点名

元组的类型词汇表加上第二个保留 token：`user`（第一个是 `literal`，ADR-0004）。类型为 `user` 的值携带 `member-{channel}-{member}`，`ingest` 把它**解析**成 `NodeRef { ns: 42, pkey: [channel_id][member_id] }`。

- 这个字符串由调用方写、由核心解析，从不作为名字存储，所以它无法与键漂移。
- **未命中时什么都不建。** 身份行是管理期的数据（ADR-0009 §1）；一次摄入点到没登记的成员，这条事实就失败，而不是凭空造一个人。这是图"缺失即建"的行为唯一错误的地方——实体可以被发现，成员只能被登记。
- 改名之后语法依然稳定，而这正是 ADR-0009 §1 当初用命名规则换来的性质。改变的只是它住在哪里：wire 上，不在存储里。

### 3. 实体 collection 不再装成员代理

`entity_by_name` 再也看不到 `member-…` 行，因为没人写它们了。一条画像事实是 `user -[PREFERS]-> iced_americano`：head 端点在 ns 42，tail 在 ns 36——同一张图、两个节点 collection，正是边载体的开放端点存在的那个场景。

### 4. 读侧得到什么，以及刻意不要什么

- 用户节点**没有向量**：`MemberRow` 不声明 `embedding`，另一个 collection 根本不在实体的落点索引里。人靠遍历进入（上下文头部、画像读），永不靠查询落在人身上。
- **不新增声明面。** 成员行靠点查（它的键）和单 channel 成员的前缀扫读出来。两者都是主键；另起一条面不会有自己的读方。
- 权重系统、事实契约与边的布局都不动。

## 为什么不这样

- **另建一张 `users` 表，键 `[scope][user_id]`。** 它会把 `MemberRow` 的字段在第二个身份下复制一份，而"人有两个 id 空间"是后来无法调和的复制。ADR-0009 §1 已经把身份层放进一张表了；缺的那一半是图的参与，不是又一张表。
- **在实体 collection 里做联合节点类型（`kind = member`）。** 那就是今天的代理，只是用字段替代了名字：人照样在落点索引里，身份照样留在抽取出来的 collection 里——上面两个缺陷原样保留。
- **在席位之上再加一层 `person`（跨 channel 画像）。** ADR-0009 §1 说画像跨 channel 有效、而席位是按 channel 的；成员节点继承了这个张力，与命名规则时期一模一样。要解决它，需要一个人物登记簿和它自己的发号权柄——那是另一个决定，不能搭这个的便车。在它之前，"一个画像跨两个 channel"就是把同一个人在这两个 channel 各登记一行成员。

## 后果

- `channel::member_node()` 作为身份退役，作为引用语法保留：同一个字符串，方向相反——调用方写，核心解析，永不存储。
- mem-core 的图路径通过 **channel keyspace** 的句柄读 members collection。两个 keyspace 是分开的（`channel`、`memory`），而 okm 的 `commit_batch` 只往它被调用的那个句柄里写，所以一条事实的批留在 `memory` 内、成员读是跨句柄的一次点查——永不进那个批。
- 成员端点靠读自己那行来渲染（显示名、角色）。于是一条 recall 行每个用户端点要多付一次点查；替代方案——把显示名冗余到边上——会把管理期数据塞进一条抽出来的行，并把改名变成一次图迁移。
- 现存的成员代理实体行以及指向它们的边（ns 36，名字 `member-…`）：属于迁移问题，不是 schema 问题——与当年遗留 flat 行同一个立场（ADR-0002）。
- **已落地（2026-10-21）。** `#[ok_nodes(EntityRow, MemberRow)]`，生成 `FactNodes { EntityRow, MemberRow }` 与 `NS_VALUES = &[36, 42]`；`ingest` 经 `parse_member_ref` 解析保留 token `user`，对畸形或未登记的引用**拒绝**（`IngestError::Refused`）且什么都不创建；端点变成写侧的 `Endpoint` 枚举，于是一条事实的批仍落在 `memory` 内、成员行是 channel 句柄上的一次点查（`GraphStore::with_channel`，由 `open_service` 接上）；`resolve_ref` 答一个 `NodeId`（`Entity { scope, node_id }` | `User { channel, member }`），人靠行上的显示名渲染；分桶只取实体端点，所以用户节点永不进社区、也永不进落点索引。收 `NodeRef` 的遍历形态（`out_edges_of` / `in_edges_of` / `typed_out_of`、`user_ref`）就是用户锚定的入口。门禁：mem-core graph 33/0（两个新测试——`a_user_endpoint_is_a_node_of_the_identity_collection`、`an_unregistered_user_reference_is_refused_and_creates_nothing`），其余套件全绿，clippy 0。kind 字段随同一批改成 `kind: Vocab<u16>`（okm ADR-0032），手写的 `kind_id` + `PREDICATE_VOCAB` 路径退役。