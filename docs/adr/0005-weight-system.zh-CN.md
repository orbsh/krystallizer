# ADR-0005: 权重系统 — delta 计数、读写比四象限、工具索引

**状态**：Accepted（设计定案，实现未开始）
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
put edge:{src}:{label}:{dst}/read/{ts}  → +1        # 只追加 delta
put edge:{src}:{label}:{dst}/count/read → N         # 归并结果，整值 put
```

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
