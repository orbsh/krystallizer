# PROMPTS — 提示词面

> **语言：** [English](PROMPTS.md)（主文档）· [中文](PROMPTS.zh-CN.md)

状态：**Declared**（草案；这里还没有任何一处接线）。提示词是调用方持有的文本；
k10r 不持 LLM（ADR-0007），所以下面这套在调用方一侧跑，不是 k10r 的资产。

两个面。§1 是调用方的工具定义；§2 是 k10r 随压缩判词一起下发的尾提示词
（ADR-0008 §4）——它的文本是 k10r 的资产，服务的那一份就是
`crates/mem-core/src/compress.rs` 里的 `TAIL_PROMPT`，与下面那段逐字一致。

本页整合的那份草案还带了一套写侧「胶囊生成器」（社区切片进，标题 +
摘要出）；它随胶囊表一起退役（ADR-0010 §4，2026-10-20）——读侧直接把社区的节点
与边渲染出来，不再请模型替它总结。

下面的命名一律按已定的词汇。源草案自造的名字或字段，标成 **[不在设计里]**、就
地留着，而不是悄悄采纳——见 §4。

## 1. 实时路径：工具定义（gravity 那一侧）

追加到 agent 的 system prompt。它的存在是为了让「添加」成为一种反射，而不是会
话结束时的冲刷，同时挡住模型存废话。

```
# 记忆：主动添加

你背后有一台图记忆引擎，通过一个工具触达。对话中一旦出现关键原子事实、架构
决策与用户明确偏好，就在轮内调用记忆添加工具把它们保留下来。

## 添加什么
不要添加闲聊、填充语、试探性推理。只在用户明确说出下列内容时添加：
- 核心工程决策——框架、存储、协议、性能约束（"我们选 Rust + Fjall 做本地存
  储，避免写放大"）。
- 不可变的规则与公理——领域逻辑或格式约束。
- 长期用户偏好——明确的状态、身份、刚性的风格偏好。

## 形态
把陈述拆成主谓宾三元组，序列化为**定长 8 元组**（JSON 列表）：三个角色按
head / rel / tail 顺序排列——head / tail 是 `[值, 类型, 属性]`，关系是
`[关系名, 属性]`（关系名就是它的种类，没有类型标签）——由调用方把这条元组交给
`ingest(fact)`。
- head / tail——值 = 实体（短、snake_case：`galaxy_repo`、`fjall_engine`）；
  类型 = 实体类型标签（小写单 token）；属性 = 实体的属性。
- rel——值 = 关系或约束（大写 snake_case：`USES`、`IMPLEMENTED_IN`、
  `PREVENTS`）；属性 = 关系的修饰（时间 / 条件 / 原因 / 强度）。
- 属性为空写 `{}`——定长，槽位不省略；类型词表开放，但**保留一个词项**：
  `literal` 标记没有自身节点身份的值（日期、数字、短字符串），k10r 把它落成
  字面量节点，它的精确文本就是它的名字。

## 例子
用户："Galaxy 仓要转向一个叫 k10r 的嵌入式 Rust 记忆核——网络图数据库对无状态
架构太重了。"
  ["galaxy_repo", "repository", {}, "PIVOTED_TO", {"reason": "network_graph_too_heavy"}, "k10r_core", "system", {}]

用户："写代码时我爱喝冰美式，但咖啡里绝对不能有奶。"
  ["user", "person", {}, "PREFERS", {"context": "while_coding"}, "iced_americano", "coffee", {}]
```

让这段提示词保持诚实的几条注记：

- **工具名属于记忆族，不是 `k10r.store()`** **[不在设计里]**。调用方的名字是
  `recall_memory` / `commit_memory`（gravity 的固定工具清单，每族一对
  (retrieve, act)）；k10r 内部的写门是 `ingest(fact)`（确定性、定长 8 元组）。
  flat 记忆的 `store` 门已撤（2026-10-19）。工具名永远不是 k10r 的函数名。
- **模型不打时钟。** `ts_ms`/`sub` 是调用方的（ADR-0008 §3）。事实若带日期，由
  调用方给——让模型"给当前 ISO 时间戳"，它会编一个 **[待裁决：事实的 `since`
  谁来盖]**。
- **`confidence` 按草案那样写不在设计里。** 设计的排序输入是权重系统
  （ADR-0005：读/写计数、`tool_invoke_count`、读写象限），加上**读侧**的 landing
  置信地板。由模型逐条声明置信度是一个提案；若采纳，它是行上的声明属性，不是
  键的一部分。
- **这里不说事实去哪。** 事实进哪张表（现在是事实图），由调用方的路由与写时 fusion
  （ADR-0004/0007）决定，不由模型决定。

## 2. 压缩尾提示词（k10r 那一侧）

随判词以 `tails: [{ name, prompt, from_*, to_* }]` 的一项下发：调用方把它接在那段
记录后面——记录**带序号**，答案才有东西可指——作为与用户那一轮并行的分支轮跑。
k10r 把摘要落在自己提议过的切分点上，所以调用方从不声明覆盖时间戳。

提示词要**两份产物**，它们回答的是不同问题。摘要是必要的：它覆盖这段。切点是
**意见**：这段是字节预算挑出来的，模型读完之后看得出对话真正断在哪，而预算看不出。
它作为**标签**记在摘要旁边（从不施加到摘要上）：切分器的训练数据，也是后续尾操作
读的范围。

```text
Two products from the conversation span above, as ONE JSON object and nothing
else.

{"summary": "...", "cut": <integer>}

1. "summary": a summary an agent can continue from alone. Keep: decisions and
their reasons, facts, names, numbers, identifiers and code verbatim, open
questions, and what the user is trying to achieve. Drop: politeness, repetition,
and reasoning that led nowhere. Do not answer the last message, do not ask
questions, do not invent anything that is not in the span.
2. "cut": the number of the LAST numbered record the summary should cover.
Choose the boundary where the conversation breaks — the end of a request and its
answer, not the middle of one. Use the last number when the whole span belongs
together. The numbers are already in the span; do not copy them into the
summary.
```

（服务出去的是单行字符串；上面按本页宽度折行。）

## 3. 这些块部署在哪

| 块 | 归属 | 跑在哪 | 接线状态 |
|---|---|---|---|
| §1 工具定义 | gravity（调用方） | 插进轮内的 system prompt，与固定工具清单并列 | 未接线（工具清单本身还在 Phase 0：纯聊天、无工具） |
| §2 压缩尾提示词 | k10r（随判词下发） | 调用方把它接在那段记录后面，作为与用户那一轮并行的分支轮跑 | 两侧都已落：k10r 的 `tails` 与 gravity 的分支轮（`compress` 一道）；切点随 checkpoint 落下 |

## 4. 来源与待核实清单

本页整合了一份外部 AI 生成的提示词草案。整合规则：**留它的提示词文本**（写得
不差），**绝不采纳它自造的架构**。被标出的事项集中在这里：

- `k10r.store()` 作为工具名——不在设计里；工具面是 `commit_memory` /
  `recall_memory`，k10r 的门是 `ingest(fact)`。
- 逐条 `confidence`——不在设计里（用的是权重系统 + 读侧 landing 地板）。
- 由模型给 `since`——时钟归调用方；事实这一项待裁决。
- "Layer 3 Semantic Capsule"——草案的编号，而且它那一整趟固化都退役了（ADR-0010
  §4）：没有胶囊，读侧直接渲染社区。所以草案那套胶囊提示词在这里一个字都没留。
- 样例实体（`galaxy_repo`、`k10r_core`）与被丢弃依赖的例子是示意，不是实测。