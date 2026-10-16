# ADR-0010: 算法改进 —— 写时消解与自动认知分箱

**状态**：Declared（设计定案，实现未开始）
**日期**：2026-10-16
**英文版**：[0010-algorithm-improvement.md](0010-algorithm-improvement.md)

## 背景

主动面（长期记忆，ADR-0001 Update 2026-10-16）有表，但没有算法。提取出来的
KDL 事实一条条到达，落成散落的节点，没有任何东西给它们秩序。秩序只在两个
地方付账：读时（每次检索自己聚类需要的邻域），或写时（在打字间隙跑一遍后
台，即调用方旁路提取本就在触发的地方，ADR-0007）。

本文档取后者：写侧把全部成本——冲突裁决、分箱、子图摘要——收进旁路内部，
于是读侧完全不跑图算法。文档合并了为这条轨写过的两份草案（散落在
`docs/LPA.md` 的一份：三表 schema、生命周期阶段、可视化延伸；以及一份写时消
解变体：裁决规则、密度边界指标、胶囊 prompt、读侧草图），并纠正了两者共同的
一处：它们把存储写成「字符串前缀 + JSON 值」，这被 okm 的建模规则否掉（键段
里出现变长文本、用手维护的前缀替代访问方法、值用序列化文本）。§1 是同三张
表按 okm 术语的表述。

下面是约束每一步的前提：

- **mem-core 不持 LLM、不持传输、不是 actor**（ADR-0007）。它内部运行的
  一切都是确定性的、本地的；产生事实的提取是调用方的旁路任务，k10r 从不
  回调任何人。
- **检索是向量着陆 + 实体名/别名精确点查，然后按权重排序的遍历**
  （ADR-0003 Update 2026-10-14）。没有词法分支可复用，也不允许再加第三套
  检索机制。
- **一个 WriteBatch = 一个事务**（ADR-0002）：本算法做的每一处索引变更都
  与数据变更同一次提交。
- **边权来自 ADR-0005 的权重模型**——不引入第二套权重语义。
- **Append-first，不是 append-only**（ADR-0002 Update 2026-10-16）：当禁止
  修改比修改本身更贵时，修改是允许的。本文档是这条规则的一个实例，不是从
  「不可变」教条里凿出的例外。

## 决策

### 0. 流水线

```
[ 步骤 1：新 KDL 事实 ] ──► [ 步骤 2：写时消解（upsert） ]
                                    │
                              (写入 Fjall)
                                    ▼
[ 步骤 5：读侧 ] ◄── [ 步骤 4：子图导出 + 胶囊 ] ◄── [ 步骤 3：增量 LPA ]
```

步骤 1–2 在旁路落一条事实时执行；步骤 3–4 在同一次后台里，于写入之后执行；
步骤 5 是读路径，不跑任何算法。

### 1. 写时规范化与消解（按 okm 术语的布局）

旁路把结构化事实（主谓宾三元组，序列化为 KDL）通过确定性的 `ingest(fact)`
门交给核心；核心不解析散文，也不调模型。这条轨的状态由三张集合承载；符合
okm 规范的布局如下（ns 号是提议值——ns 字典即代码，okm ADR-0002——在表落地
时分配，取 35 之后的空号）。

```text
实体 —— 节点集合（提议 ns 36），每图一个
  0x0 主表           [scope u64][node_id u64]
                     → 行体：kind_id（热）、community_id（热）、
                       name（冷 TLV）、props（动态段）
  0x1 节点种类面     [kind_id u16][scope u64][node_id u64]        声明式索引
  0x1 by_name 面     [name][scope u64][node_id u64]               实体名/别名精确入口
  0x1 by_community   [community_id u64][scope u64][node_id u64]   ← 草案的 index:community:

事实 —— 边集合（提议 ns 37），每图一个 ns（okm ADR-0017）
  0x0 主表           [scope u64][edge_id u64]
                     → 行体：[src NodeRef][dst NodeRef][kind_id][since][confidence][weight]
  0x1 声明字段面（since / confidence / weight 各一面）
                     [字段值][scope u64][edge_id u64]
  0x4 种类索引       [kind_id][scope u64][edge_id u64]
  0x5 出边           [src NodeRef][scope u64][edge_id u64]
  0x6 入边           [dst NodeRef][scope u64][edge_id u64]
  0x7 种类+出        [kind_id][src NodeRef][scope u64][edge_id u64]
  0x8 种类+入        [kind_id][dst NodeRef][scope u64][edge_id u64]

社区 —— 胶囊集合（提议 ns 38）
  0x0 主表           [scope u64][community_id u64]
                     → 行体：title（冷 TLV）、summary（冷 TLV）、
                       updated_at（热）、state（热：pending / done）
```

`scope` 领在每个主键前段，因为语料按 user 分（ADR-0003）——那是有属边界，
与 flat memory（`[user_id][id]`，ns 32）同形，不是优化。`NodeRef` 是
`[ns 2B][len varint][pkey]`，自描述（okm ADR-0017 §2），因此任何节点集合都
零声明参与。

与草案不同的五处，以及原因：

- **名字永不进键，也不往键里塞哈希。** `KeyEncode` 在编译期拒绝 `String`；
  把哈希塞进定宽段会静默撞车（撞到就是寻址到另一个实体的行）且渲染不回来
  ——这正是 okm 用代理 id 的理由（MODELING「Open vocabularies」）。实体名住
  在 `by_name` 面和行的冷段里，路径是 `name → node_id`。草案的
  `community_id = hash(entity_name)` 种子与 `c_milkyway_2026` 式 id 一并去掉：
  一个社区是胶囊集合里的一行，其 id 由 `HighWater` reduce 发放（单写者，永不
  复用），人类可读的标签就是胶囊的 `title`。
- **每个查询维度都是声明式访问方法，而不是埋在键里的前缀。** 社区反向索引
  就是 `by_community` 面（`[community_id][scope][node_id]`），它的尾部就是主
  键编码——可解码，符合 okm「索引条目的尾部可解码、主键的自定义尾部不可解
  码」这条规则。手写前缀方案正是这条规则要防的东西。
- **这里任何地方都不用 `[order]` 段。** 主表读都是点读与遍历读，不是复数有
  序范围读，而排序键只有在结果是复数时才配得上它占的字节（okm MODELING
  「Key naming」）。`since` / `updated_at` 是 payload 字段，遵 ADR-0005 自己
  的先例：可变元数据永不进键。
- **种类是运行时数据，算法要查的三个边字段是声明字段。** 谓词种类通过边集
  合自带的字典分配（0x2/0x3，name → kind_id）；而 `since` / `confidence` /
  `weight` 是 k10r 自己 schema 的声明字段，各得一个 0x1 面（okm ADR-0017
  §3）——在 okm 的设计里这两者正交。提取出来的、开放式的属性走动态段，需要
  查询时通过 path 索引（okm ADR-0012 / ADR-0029）到达。
- **值用原生段，不用 JSON 大块**（okm ADR-0018）：定宽字段连续（热），变长
  文本作冷 TLV 段。

新落地的事实是一个散落节点，带一个独立种子标签——种子是一个新社区 id，不是
哈希。

**消解：在事实写入前先找在场者。**

```
                        新事实传入
                             │
                             ▼
   扫一次 0x7 面：这个源出去的、这个种类的边有哪几条？
                             │
          ┌──────────────────┴──────────────────┐
          ▼ (没有)                              ▼ (有在场行体)
      新 link 一条边                  执行裁决：
      （平行边是各自独立的事实）      new.since > old.since
                                     OR new.confidence >= old.confidence ?
                                            │
                          ┌─────────────────┴─────────────────┐
                          ▼ (是)                              ▼ (否)
                    原地改写行体                        丢弃新事实
                    （同一个 edge_id）                  （或降低其权重）
```

草案的两处记账在 okm 的机制下消失：

- **不用手删索引条目，也不用墓碑。** 草案得在同一个 batch 里手动删掉旧索引
  条目；okm 的声明式面是从行派生出来的，改写行就重生成它的面条目（weight /
  since / confidence），陈面不可能残留。
- **平行边是各自独立的事实**（okm ADR-0017）：同一个 (src, kind) 配不同客体
  是另一条边、另有 `edge_id`，所以覆写只适用于「在一个源上函数式」的关系种
  类——种类如何声明其基数见「欠账/未决」。

```rust
// 草案草图 —— mem-core，在一条新提取的三元组上（按 okm 形态）
let kind = kinds.id_of(predicate);   // 0x2/0x3 字典：name → kind_id
let src = NodeRef::new(&ENTITY_NS.to_be_bytes(), &subject_id.to_be_bytes());
let dst = NodeRef::new(&ENTITY_NS.to_be_bytes(), &object_id.to_be_bytes());

// 1. 这个源出去的、这个种类的边：扫一次 0x7 面
let incumbent = edges.typed_out(predicate, &src).next();

let mut batch = edges.batch();
match incumbent {
    // 2. 与在场行体做裁决
    Some(edge_id) => {
        let old = edges.get(edge_id);
        if new_fact.since > old.since || new_fact.confidence >= old.confidence {
            // 原地：同一个 edge_id，行的面随行而来
            batch.put(edge_id, new_fact.into_body(src, dst, kind));
        }
        // 否则：丢弃，不写
    }
    // 3. 无在场者：link 一条新边（0x4/0x5/0x6/0x7/0x8 同批）
    None => batch.link(EdgeFact { src, dst, kind, ..new_fact }),
}
batch.commit();   // 一个 WriteBatch = 一个事务（ADR-0002）
```

### 2. 增量标签传播（LPA）

Append-first 的写入产出的是堆，不是秩序；秩序按每次写入在内存里局部划定，
而不是靠一遍全局。

**为什么用局部增量 LPA。** 它的成本在受影响的邻域上接近 O(E)，不需要把全
量大图装进内存，而且它正是 ADR-0003 里说的那种值得引依赖的算法（社区发现
在那里被点名为「简单算法直接实现」的例外）。内存图的载体是 `petgraph`。

机制：

1. **构建局部子图**——通过遍历面（0x5/0x6、0x7/0x8）载入新写入的节点及其
   直接相连的 2-hop 邻居，在内存里组装成一张临时图。不载入全量图。
2. **邻居投票**——节点审视邻居的 `community_id`，以边 `confidence` 乘累积
   边权投票：
   `Target_Community = arg max over C of Σ_{e ∈ Edges(C)} Weight(e) × Confidence(e)`
3. **收敛与提交**——常见情形 3 轮足够收敛（草案允许 3~5 轮）。胜出的
   `community_id` 写进成员节点行的字段；`by_community` 面随行而来，所以重新
   分箱一个节点既不需要墓碑，也不需要第二次索引写。胶囊行的 `state` 置为
   `pending`。全部走同一个 WriteBatch（ADR-0002）。

### 3. 子图边界判定

LPA 划完箱子，接下来的问题是：一个箱子什么时候值得摘要，才能不让一条新事
实就触发一次 API 调用。

**指标——密度水位线：**

`Density = 2 × |E| / (|V| × (|V| − 1))`

其中 `|E|` 是同一个社区内部的边数，`|V|` 是该社区内的实体节点数。

**触发。** 满足任一条件即导出该社区做摘要：(a) 新事实并入使其密度越过配置的
阈值；或 (b) 其内部未压缩的新事实数越过水位线（草案给的 15 条）。

**导出。** 扫一次 `by_community` 面，整桶捞出该社区的成员；它们的边通过遍历
面取回，配合内存里的成员判定——这就是该社区的完整子图，切片成一个纯文本
KDL 包，即下一步的输入。

### 4. 胶囊固化（第三层）

这是从第四层（细碎图谱事实）升到第三层（宏观语义理解）的桥。

**这次调用是调用方的，不是核心的。** k10r 不持 LLM、不持传输、从不外呼
（ADR-0007），所以它自己的部分只有那个包，以及社区上的 `pending` 标记；推理
本身由拥有该旁路的消费方自己发起（聊天消费方是 gravity，memory-primary 消费
方则是它自己的单遍 worker）。草案把它写成「通过 gravity 提供的非阻塞通道向
外抛出」——那说的是调用方用哪条载体，不能被读成 mem-core 的外呼。

**Prompt（照草案原文保留；输入是 KDL）：**

```
你是一个图谱语义提炼器。以下输入是某个特定知识社区内的全部 KDL 原子事实片段：
------------------------
${KDL_SUBGRAPH_PAYLOAD}
------------------------
请不要发散，直接执行两项任务：
1. Title: 为这个知识社区拟定一个 10 字以内的精准概念主题。
2. Summary: 将上述零碎的三元组关系融会贯通，高度压缩提炼成 2 句话的"语义胶囊摘要"，捕获核心架构、因果决策或用户偏好。
```

**回写是对胶囊行的点覆写**（一个社区一行，用社区自己的 id 作键）：

```text
0x0 主表  [scope u64][community_id u64]
          → { title, summary, updated_at, state = done }
```

覆写本身就是要害：胶囊是缓存——只有最新的摘要有阅读价值，而历史正是那些事
实自身已经承载的东西。一个社区一行也让键的数量由社区数封顶，而不是每重构一
次摘要就长一行。

### 5. 读侧（写侧成本买到了什么）

问题到达时，冲突已在写时消解、`by_community` 面已维护好、宏观摘要已物化。
于是读路径就是：着陆、读胶囊、扫桶、组装——而每一步都是具名的访问方法。

```rust
// 草案草图 —— 读路径不跑图算法
pub fn assist_search(query: &str, top_k: usize) -> AssistantContext {
    // 1. 进程内本地向量着陆（top-k 种子；实体名这类标识符走 by_name 精确入口
    //    —— ADR-0003 Update）
    let landed = local_embedding_engine.seeds(query, top_k);
    let cid = landed.community_id;                 // 节点行上的声明字段

    // 2. 预先算好的第三层胶囊：一次点读（零 LLM 成本）
    let capsule = communities.get(CommunityKey { scope, community_id: cid });

    // 3. 社区桶：扫一次 by_community 面取成员，边走遍历面（0x5/0x7）
    let members = entities.scan(by_community(cid));
    let facts = edges.within(&members);

    // 4. 组装成注入调用方 prompt 头部的辅助上下文
    AssistantContext::new(capsule, members, facts)
}
```

全路径是一次着陆加具名读（一次点取、一次面扫描）；本来会挡在每个问题前面的
图拓扑工作没有了。注入的包在两次固化之间是稳定的——同样的事实、同样的胶囊
——草案据它指望 provider 侧的 context cache 复用：社区的事实与胶囊在下一次
改写前不变。

### 6. 前端延伸（不新增机制）

持久化的 `community_id` 可以零成本交给可视化：确定性的 KDL → JSON 转换（不
经过 LLM）喂给 **3d-force-graph**——WebGL/three.js 的力导向图库，带轨道式相
机控制，即本仓 `docs/kg-demo-3d.html` 已经在用的那条载体。那个 demo 把节点/
边的数据包流式推入运行中的布局，并按批重新加热模拟，而不是重画整棵树，正好
就是这里的生产者形态。节点颜色绑定 `community_id`（一个社区一个颜色簇）、
节点大小绑定权重，于是长期记忆里秩序的生长——分箱本身——可以从读
`get_stats` 的仪表盘上看出来。

## 后果

- **本文档是 append-first（而非 append-only）的活例。** 边行体与胶囊行被覆
  写，因为把一条已被取代的事实留成第二行更贵：它会同时毒害检索和读它的分箱。
  日志形状的表（ns 33/34）不受本文档消解步骤的影响。
- **本设计里每一次读都是声明式访问方法**（okm：「访问方法是强制的」）：桶是
  `by_community` 面，消解查找是 0x7 面，精确入口是 `by_name`。不引入裸前缀
  扫描，也不引入通用全表扫描面。
- **键里不带文本、不带哈希值。** 实体名与社区标签住在 payload 字段与索引段
  里，因此能渲染回来、能按人类可读的方式枚举；身份是由水位发放的代理键，永
  不是塞进键段的哈希。
- **不新增检索机制。** 社区索引是事实图上的一个投影，胶囊是一次缓存摘要读，
  从不是计分分支；检索仍由着陆或实体名/别名精确点查进入（ADR-0003 Update
  2026-10-14）。
- **投票消费 ADR-0005 的权重**（`Weight(e)`、`confidence`、读写四象限）。
  ADR-0005 里写作 `W:{weight}:{edge_id}` 的那个权重索引，就是同一条规则往下
  一层的表述：边集合的一个声明字段面（`[weight][scope][edge_id]`）。
- **这里的每个数字都是策略参数**，属于 `config/krystallizer.kdl`（ADR-0008
  的规则）：密度阈值、未压缩事实水位线、迭代上限、2-hop 半径。提取水位线
  （草案里的 100 条消息）不是 k10r 的——它在调用方的旁路里触发（ADR-0007）；
  k10r 只接手递给它的事实。
- **重划分在结构上就是便宜的。** 一个节点换社区只改写一个行字段，它的面条目
  随之更新；反复分箱永远不会变成跨越一次扫描的读改写，也不会等文件锁。

### 欠账/未决（本文档未定案的部分）

- **事实图的表是 Phase 3 的声明项；本文档定的是它们的键设计，不是它们的落地。**
  提议的 ns 36/37/38 在表落地时经字典分配。
- **函数式 vs 多值种类。** 覆写规则只对「在一个源上函数式」的种类成立
  （`priority`：一个实体一个值）；多值种类合法地持有平行边。种类如何声明其
  基数（种类字典里一个标志位、一个声明字段，还是 `ingest` 上由调用方给的标
  志）未定。
- **社区的承载位置。** 两份草案互相矛盾——`community_id` 在事实行上（它们的
  消解/索引代码）还是在实体节点上（它们的 LPA 与密度指标）。本文档放在
  **节点**上（LPA 在节点之间投票，`|V|` 数的也是节点），社区的边通过遍历面
  取回；若注入的包必须是边本身，边侧再开一个 `by_community` 面是备选。
- **字面量客体。** 提取出的客体若是日期、数字或短字符串，它没有节点身份，
  而 okm 的图边只承载 NodeRef 端点。它成为一个字面量节点（种类 `Literal`，
  值放 payload，经 `by_name` 可达），还是边上的一个声明属性字段，归 Phase 3
  决定。
- **胶囊调用的载体。** 由于 k10r 不能外呼，固化请求只能由调用方拉取或被标
  记给它。具体哪扇门承载（记忆面上一次「待固化」读，还是调用方自己的一次推
  送）未定。
- **社区 id 的发放**走 okm 的发放者纪律（`HighWater` reduce，单写者）；该机
  制随表落地。
- **草案的前缀布局是作废，不是保留**：手维护的 `node:` / `index:community:`
  前缀在上文被 okm 的面取代，于是草案「一个字段两种键形状」的矛盾被关掉，而
  不是被继承。
- **ADR-0002 与 MODELING 仍把事实图写成 `edge:{src}:{label}:{dst}` +
  `EdgeTable`**——对照 okm ADR-0017 两处过时。改写那几行是 Phase 3 落地时的
  欠账，本文档不动它们。
- **与 PLAN 的关系。** 社区发现位于 Phase 5+（「dreaming consolidation——
  petgraph 社区发现 + 人工审核的操作集，不要提前开工」）。本文档说的是它的
  增量局部形态，搬进了写旁路。这是否把 Phase 5 的一块提前到 Phase 3/4 之前，
  是一项裁决，不是本文档的后果。
- **照草案搬过来、本文档未实测的说法：** 毫秒级读延迟、provider 侧
  context cache ≥90% 命中率、「Rust 内核代码量缩减一半」。它们是这个形态的
  理由与目标，不是结果。