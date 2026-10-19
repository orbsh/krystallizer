# ADR-0005: 权重系统 — 计数在边行上、读写比四象限、工具索引

**状态**：已落地 2026-10-19（计数住在边这一行上；delta 追加表只作为多写者的退路保留）
**日期**：2026-09-08
**英文版**：[0005-weight-system.md](0005-weight-system.md)

## 背景

graph-memory 设计为每条 fact 边配一层系统权重元数据（区别于业务
属性）：read_count、write_count、tool_invoke_count、last_read、
last_write。读写比划分四象限（核心区 / 深水区 / 噪声 / 冷区），检索
顺序按权重 Top-K（渐进式加载）。

两个工程问题：如何计数才没有 read-modify-write 争用；如何服务按权重排的 Top-K
才不全表扫描。

## 决策

**计数：边这一行就是计数键。** `read_count` / `write_count` / `tool_count` /
`last_read` / `last_write` 是 `FactEdge` 的声明热字段（ns 37，payload 布局 v2），
fold 出来的 `weight` 就住在它们旁边。一个身份、一次写——而 okm 的声明面是从行派生
的，所以加一次计数只重写一行，派生的一切随之跟上。

- **谁加谁。** `write_count` 是事实自己的断言：`ingest` 在 link 或赢得裁决时加一
  （被丢弃的事实不算写）。`read_count` 是送出的 recall——只有调用方带着时钟
  （`ts_ms`）时 `recall` / `expand` 才加，因为没人盖戳的读不算读，缺失的时间戳不能
  凭空生出衰减。`tool_count` 是调用方上报的工具调用
  （`POST /facts/{scope}/edges/{id}/touch`）：k10r 看不见内部的工具调用，用过这条
  事实的消费者自己报。
- **read_count 与 write_count 物理上保持分开**——原子事实追加模型保证读写独立，
  这正是二维四象限分类在存储层稳定的前提。
- **fold 点在访问处，不在后台。** 一次 touch 用一个 batch 重写这一行。期间累积的
  衰减在**这次** fold 结算（对着上一次访问量），同一次 fold 再把时钟基准前移，所以
  这段间隔只收一次、不会重复收。**顺序是承重的**：先对着上一次访问 fold，再推进时钟
  与计数——先推时钟再 fold 会让 `time_decay` 永远是零，而这个错误在数字上不可见
  （结果只是所有事实按原始计数排序）。delta 追加表仍是多写者的**退路**：它折进同样
  的字段，切过去不改 schema，落地时唯一要钉的是它的 key 布局。

**权重公式**（沿用 wiki，含工具索引）：

```
weight = read_score + write_score + tool_score + rarity_bonus - time_decay

tool_score  = log(1 + tool_invoke_count) × 1.5
read_score  = log(1 + read_count)
write_score = log(1 + write_count) × 0.5
rarity_bonus = 1.0 if read_count > R AND write_count < W else 0
time_decay  = days_since_last_access × 0.01
```

深水区加成（高读低写）是承重墙：写阈值惩罚会结构性打击高录入成本的
专家知识，rarity_bonus 是对它的纠正。

- **桶。** 存的是公式的毫单位（× 1000，四舍五入，下界 0）：小浮点要有一个稳定的
  有序整数才能按整数比较，而 0.001 比每日 0.01 的衰减步长更细，所以分桶不会把公式
  本来分得开的两条事实排成同序。系数是 `mem-core/src/graph.rs` 里的命名常量
  （读 1.0、写 0.5、工具 1.5、稀有 1.0、衰减 0.01/天、深水区门槛 read > 3 且
  write < 3）。它们是公式本身，不是部署旋钮；要做策略层就开一个 config 节点，
  而现在没有读者。
- **排序。** `recall` 的种子保持相似度序，事实按权重降序（稳定排序，等权时保留
  把它拉进来的种子序）。着陆仍归 ADR-0003，权重归本 ADR。

**Top-K：不声明任何权重面，也不存在索引。** `W:{weight}:{edge_id}` 答的是「全库
最重的边」，不是「这个用户最重的事实」：边没有 `scope` 段（ADR-0010 §1，归属走
端点）。所以 scope 级的 Top-K 是一张按 `[scope][weight][edge_id]` 的**投影表**、
随同一次 touch 维护——等消费它的那个读（Phase 4 的渐进式加载）存在时再命名，不是
提前声明的 face。recall 的排序不需要它：要排的集合已经在手上了。这个面不声明的另外
两个理由：权重变更本来就要重写这一行，为一个不存在的读者维护 face 是白付；而 okm 的
文档覆写**不会**删掉按**旧值**建的那条索引条目，于是每次加计数都欠一次显式删除，
只为一次没人发的查询。

## 后果

- 今天的排序是对 recall 已经取回的那个集合做内存排序——没有索引、没有全表作业、
  没有第二个引擎。
- 冷层作业（Phase 5+ 的淘汰）与渐进式加载要的是同一张 scope 级投影表；哪一个先到，
  一张表同时服务两者，而它的形态上面那条决策已经写明。
- 时间衰减使一条事实的权重在两次 fold 之间非单调，这是可接受的：今天没有任何派生
  状态要跟着它同步，排序也容忍天级陈旧（与 skill 边界重构同一 eventually-consistent
  姿态）。

## 历史

**2026-09-08 的草案，在计数这半落地时（2026-10-19）退役。** 它按追加 delta 计数
——`put edge:{src}:{label}:{dst}/read/{ts} → +1`，后台 fold 进
`put edge:{src}:{label}:{dst}/count/read → N`——并从一张声明的权重二级索引
`W:{weight}:{edge_id}` 服务 Top-K，每次权重变化写新键、删旧键（tombstone）。

- 那些 delta 键错了两次：一是写在一个作废的**层**上（`edge:{src}:{label}:{dst}` 不是
  k10r 的键形态——边的身份是 `[edge_id]`，端点与字典分配的 `predicate_id` 以 ref
  承载）；二是 delta 表本身对单写者毫无收益，而存储今天就是单写者。
- 计数键也不需要旁表：计数器就是它所计的那一行的字段，所以一次 fold 是一次行重写。
- 声明的权重面死因与上面 Top-K 那条相同：它服务整个库而不是一个 scope，而且它会
  为一个不存在的读者被维护。tombstone 规则随之一起走——没有面留下，也就没有东西会
  悬空。