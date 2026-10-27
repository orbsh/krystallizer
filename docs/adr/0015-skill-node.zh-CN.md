# 0015 — skill 节点：解析器列出的那一行，执行器走的那些步

状态：Accepted（2026-10-26）

## 背景

gravity 的工具清单是固定写死的（gravity ADR-0004），其中四个名字之一是
`resolve_skills`——当 prompt 与 session head 都不够用时，模型调它：调用返回候选 skill，
模型挑一个，`invoke_skill` 再去走它。这个名字今天没有门，原因不是工作量。

它在 2026-10-20 随 `GET /code`（bring-up 期的池读）一起撤掉，理由是「清单随 skill 节点
一起来，不提前」。skill 节点属于 Phase 5+。现在客户端有了，所以工具清单的第二组卡在
**schema** 上，而不是卡在调用方——本记录就是把那个 schema 定下来。它刻意只取能让那两个
调用成立的最小一片，把 planner、排序和 consolidate 留在原处。

## 裁决

### 1. skill 是事实图里 kind 为 `skill` 的实体节点

`EntityRow`，ns 36，与事实同一个 `scope`，`kind = "skill"`。

解析器要回答的问题是「这些里哪一个对得上眼前这件事」，这本来就是图的检索问题——向量早
就建立在 `title` + `content` 上（ADR-0003，2026-10-23 修订），landing 机制也早就在回答
「这段文本附近有什么值得读」。另开一张表就要第二个索引、第二条检索路径，再加一次与
skill 经验所挂事实的 join；而图这三样都已经有了。

它不是 code 行（ns 35）：ADR-0012 的 Context 已经把 skill 与 script 分开——一个 skill 可以
点多个 script，一个 script 也可以服务多个 skill，而那张行表存在的意义正是让这个链接是个
链接。

### 2. 身份是节点的 `name`，声明是幂等的

skill 的把手是它的 name（`by_name` 面，与别的属性一样是属性）。名字已存在的声明**就是**
那个 skill：props 合并，门返回它落到的 node id。没有 skill id 空间要分配，没有 version 列——
skill 就是一行、说它当前是什么；要历史的时候，历史是第二个节点加一条「后者接替前者」的
链接（ADR-0012 的同一论证、同一形状）。

实体名本来就在 scope 内按解析顺序唯一，skill 继承这一点，不需要自己的一套。

### 3. 四个部分，各自住在哪

- **`preconditions`**——TEXT JSON 字符串数组：第一步之前必须成立的事。谁规划谁去查（用
  `recall_memory`）；求值不是 store 的职责。
- **`steps`**——TEXT JSON 对象数组，按执行顺序：`[{"code_id": 7, "note": "…"}]`。可执行的
  身份是 ns 35 的那行，所以 skill 永不复制 script 的语言与参数定义。
- **`verification`**——TEXT JSON 字符串数组：一步之后、下一步之前要检查什么。
- **`experience`**——**不是** prop。它是 skill 节点的**关联事实**：跑过什么、结果如何、
  在什么上面失败过。权重系统（ADR-0005）本来就在数 read/write/tool 并按时间衰减，经验
  搭的是已有机制，每次 touch 都会重新折进权重。写成 JSON blob 就是第二份记忆：既不参与
  权重，也没人衰减它。

三个作为 prop 的用 TEXT JSON，理由与 ADR-0012 §2 对 `parameters` 说的一样：store 不解析
它，形状由生产方拥有。一个 store 要解析的 prop 就是第二份要迁移的 schema，而图的契约
（`name` / `kind` / props）没有理由知道「一个 step」是什么。

`title` 与 `content` 仍是约定俗成的两个 prop（MODELING 那一节）：`content` 是 skill 的
「它做什么、什么时候该找它」，也就是检索面——不带 content 声明的 skill，只有按名字精确
查找才找得到。

### 4. 三道门，scope 与图一致

| 门 | 请求体 / 答复 |
| --- | --- |
| `POST /skills/{scope}` | `{name, title?, content?, preconditions?, steps?, verification?}` → `{node_id, created}` |
| `GET /skills/{scope}/{node_id}` | 带四个部分的 skill；未知 id 404，节点不是 skill 400 |
| `POST /skills/{scope}/resolve` | `{query, limit}` → `[{node_id, name, title, content, steps, score}]`，最优在前 |

`resolve` 走图的检索，只留 `skill` 这个 kind。检索什么都没落着时——没挂 embedding 引擎，
或者没有东西落在 landing 距离之内——退回 `by_kind` 面的 id 顺序扫描，按 `limit` 截断
（默认 5，上限 20）。因为部署没有引擎就答「没有 skill」是错误答案，而这样一份语料按构造
就不大，扫一遍很便宜。

每个候选都带着 `steps`，因为**选**就需要它：消费者是面前摆着一列候选的模型，而「这个会让
我做什么」是挑选的一部分。`preconditions` 与 `verification` 不带——那两样是选定之后、走
的时候才读的。替代方案是加一次调用（「读这个 skill」）或给 gravity 的固定清单加第五个名字；
在 resolver 本来就要回答的那一行上加一个字段，比这两者都便宜。

`resolve` **不收**调用方时钟、**不计数**。它返回的是候选，只有模型挑中的那一个才算一次
使用，而使用是在 skill 真正被走的时候计数——没人选的候选不是一次读（ADR-0005 的规则：
没盖时间戳的读不算读）。

### 5. 这不是什么

没有 planner，没有拓扑排序，没有重试策略，没有从一次运行回写权重，没有 dreaming
consolidation。本记录只定「解析器读的那一行、执行器走的那些步」。决定顺序、重试失败的步、
把结果折回经验，仍然是 gravity 的（ADR-0004 §3：一次调用走一步，顺序由模型驱动）与
consolidation 层的（Phase 5）——k10r 没有理由知道一个 step 是什么意思。

## 否决项

- **skill 作为事实（边）**：一个 skill 不是一条断言。切成几条边它就需要一个没人索引的形状，
  「解析一个 skill」变成没有落点的遍历。
- **skill 放在 code 行里**：两者是多对多，硬合会让每个共用同一个 script 的 skill 变成它的副本。
- **一个 prop 里塞一整份 JSON 文档**：对向量不透明（向量只读 `title` 与 `content`），除非再
  复制一份到 content；而且四个部分有三个不同的消费方——planner、executor、consolidation——
  各自会按不同的方式解析同一坨 blob。
- **给 skill 留一个专门的 ns**：同一个 scope 内图本来就是一份语料，`kind` 就能区分；另开 ns
  只是多一个要跟着同步的 keyspace。
- **version 列，或一个 skill id 空间**：ADR-0012 对 code 行的同一论证。
- **在 k10r 里给 steps 排序**：顺序是执行策略。store 只回答「这是那一行」；谁走、按什么顺序走，
  是执行器的判断。

## 后果

- 没有 `steps` 的 skill 合法且无用：解析器可能把它交给模型，执行器却发现没东西可跑。这个失败
  在调用时才现形（gravity 侧），不在这里——store 不负责评判一个 skill 有没有用。
- skill 跟着事实图的权重与衰减走，所以没人解析的 skill 会像语料里其他东西一样衰减。如果这被
  证明是错的，修法是在使用路径上补一次 `Touch`，不是另开一个 store。
- `content` 变成承重字段：只声明了名字的 skill，只有按名字精确查找才找得到。声明门不强制它
  （合法的节点只需一个字段），而跳过它的部署会看到解析器答得很少，然后回头去看自己声明了什么。
- `GET /code` 的池读保持撤销。skill 按 id 指名它的脚本，调用方只读它被告知的那几行——当初
  那张表提供的清单，现在是 skill 的 `steps`，而 steps 说明的是「哪个脚本属于哪个 skill」，
  这是 code 表从来不知道的事。