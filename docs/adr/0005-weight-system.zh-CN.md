# ADR-0005: 权重系统 — delta 计数、读写比四象限、工具索引

**状态**：Accepted（计数这一半 2026-10-19 落地——见「更新」；delta 表等多久写者切换）
**日期**：2026-09-08
**英文版**：[0005-weight-system.md](0005-weight-system.md)

## 背景

graph-memory 设计为每条 fact 边配一层系统权重元数据（区别于业务
属性）：read_count、write_count、tool_invoke_count、last_read、
last_write。读写比划分四象限（核心区 / 深水区 / 噪声 / 冷区），检索
顺序按权重 Top-K（渐进式加载）。

两个工程问题：如何计数才没有 read-modify-write 争用；如何服务 Top-K
才不全表扫描。

## 决策

**计数：delta 追加 key，后台 fold 到计数 key。**

```
put edge:{src}:{label}:{dst}/read/{ts}  → +1        # 追加优先：这条路径仍是追加
put edge:{src}:{label}:{dst}/count/read → N         # 归并结果，整值 put
```

左边那个键是早先一轮的写法：`edge:{src}:{label}:{dst}` 是**作废的层**，不是要
恢复的布局——图的边是一个 okm 边集合，身份是 `[edge_id]`，端点与种类以 ref 和
字典分配的 `predicate_id` 承载（ADR-0010 §1），所以这两个计数键在该系统落地
（Phase 5）时建在那个身份上。这条裁决本身没变：以追加 delta 计数、后台 fold 成
整值计数键，并且键布局从第一天起就是 delta 兼容的，多写者切换不动 schema。

- 读取时归并 delta 链得当前计数；低峰期 fold 把归并值写入计数 key。
- 单进程起步可对计数 key 直接 RMW（按 wiki 的并发模型判据：同一
  WriteBatch 串行更新——delta 机制暂无收益）。**但 key 布局从第一天
  就按 delta 兼容设计**，多 agent 切换（SlateDB+S3 共享真理源、并发
  写者）不改 schema。
- read_count 与 write_count 物理上是两个对象——原子事实追加模型保证
  读写独立，这正是二维四象限分类在存储层稳定的前提。

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

**Top-K：权重二级索引 `W:{weight}:{edge_id}`。**

- 权重变化在同一 WriteBatch 内写新 key + 删旧 key（tombstone）——记忆
  权重更新是低频批量刷，tombstone 开销可忽略（按 wiki 的索引更新判据
  选标准 put+delete）。
- 渐进式加载按索引降序读，回取边 payload，取满 K 停。
- 权重按索引需要的精度存储（分桶）：重算顺序必须与索引顺序一致，
  索引值就是 fold 时公式产出的同一个值。

## 后果

- 热路径（search）永不全扫边；冷数据识别（噪声/冷象限）是索引范围
  读，不是全表作业。
- S3 冷层淘汰（Phase 5+）从同一索引的权重底部读，不需要单独的
  "冷数据"记账。
- 时间衰减使权重在两次 fold 之间非单调——可接受：索引按 fold 节奏
  刷新，检索排序容忍天级陈旧（与 skill 边界重构同一 eventually-
  consistent 姿态）。

## 更新

**2026-10-19 —— 计数这半落地，按 okm 的形态。** 计数 key 就是边这一行，不是旁边的表：
`read_count` / `write_count` / `tool_count` / `last_read` / `last_write` 是
`FactEdge` 的声明热字段（ns 37，payload 布局 v2），`weight` 是它们旁边那个 fold 出来的
字段。一个身份，一次写。

- **谁加谁。** `write_count` 是事实自己的断言：`ingest` 在 link 或赢得裁决时加一
  （被丢弃的事实不算写）。`read_count` 是送出的 recall——只有调用方带着时钟
  （`ts_ms`）时 `recall`/`expand` 才加，因为没人盖戳的读不算读，缺失的时间戳不能
  凭空生出衰减。`tool_count` 是调用方上报的工具调用
  （`POST /facts/{scope}/edges/{id}/touch`）：k10r 看不见内部的工具调用，用过这条
  事实的消费者自己报。
- **fold 点在访问处，不在后台。** 单进程起步按本 ADR 自己的允许直接 RMW：一次访问
  一个 batch 重写这一行。期间累积的衰减在**这次** fold 结算（对着上一次访问量），
  同一次 fold 又把时钟基准前移，所以只收一次、不会重复收。delta 追加表仍是多写者
  的退路，它折进同样的字段，切过去不改 schema——落地时唯一要钉的是它的 key 布局。
- **桶。** 存的是公式的毫单位（× 1000，四舍五入，下界 0）：小浮点当索引要稳定的
  有序整数，0.001 比每日 0.01 的衰减步长更细，所以分桶不会把公式本来分得开的
  两条事实排成同序。系数是 `mem-core/src/graph.rs` 里的命名常量（读 1.0、写 0.5、
  工具 1.5、稀有 1.0、衰减 0.01/天、深水区门槛 read > 3 且 write < 3）。它们是
  公式本身，不是部署旋钮；要做策略层就开一个 config 节点，而现在没有读者。
- **排序。** `recall` 的种子保持相似度序，事实按权重降序（稳定排序，等权时保留
  把它拉进来的种子序）。着陆仍归 ADR-0003，权重归本 ADR。

**W 索引——更正。** `W:{weight}:{edge_id}` 和别的声明面一样，而它今天是被**故意
不声明**的：权重变更本来就要重写这一行，为一个不存在的读者维护 face 是白付。草案
点名的两个读者是"作用域内的 Top-K / 渐进加载"和 Phase 5 的冷层范围读。前者今天
根本不成形：边没有 `scope` 段（ADR-0010 §1，归属走端点），`[weight][edge_id]`
答的是"全库最重的边"，不是"这个用户最重的事实"。真要 per-scope Top-K，那是一张
按 `[scope][weight][edge_id]` 的投影表、随同一次 touch 维护——等这个读存在时再
命名的新设计件，不是提前声明的 face。recall 的内存排序不需要它：要排的集合已经
在手上了。
