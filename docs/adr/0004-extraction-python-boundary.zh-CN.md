# ADR-0004: 提取管线留在 Python — 核心零 LLM 依赖

**状态**：Accepted（设计定案，实现未开始）
**日期**：2026-09-08
**英文版**：[0004-extraction-python-boundary.md](0004-extraction-python-boundary.md)

## 背景

graph-memory 设计定义了两阶段提取管线（规范化 → 分类提取为原子事实），
两个阶段都有 LLM 调用和成本策略（阶段 1 小模型、阶段 2 强模型、短输入
合并为单次调用）。写入时融合流程（域外实体收敛）也涉及对小候选集的
一次 LLM 决策。

问题：LLM 编排放 Rust 核心里，还是 Python 层？

## 决策

**mem-core 接收结构化 KDL fact；它永不调用 LLM。**

- 两阶段管线、实体融合的 LLM 决策步骤、主动记忆判据（针对性 × 概括性）、
  一切 prompt 工程都在 Python 层（skillforge 侧，后续是 mem 适配包）。
- 核心的入库面是确定性的：`ingest(fact)`，其中 `fact` 已从 KDL 解析
  （`head <类型> <实体值> [props] / rel <关系> [props] / tail <类型>
  <实体值> [props]`）。
- 融合在核心内按此拆分：
  - **域内实体**（组织成员、项目、任务、商品）：稳定 id 确定性直查——
    零 embedding、零搜索、零 LLM。
  - **域外实体**：核心提供便宜的一半——embedding 检索返回候选事实
    **及其锚定的实体节点**——调用方（Python）跑昂贵的一半（LLM 判断：
    复用节点 / 合并去重 / 新建节点），再带着决策调 `ingest`/`link`。

## 理由

- 核心保持确定性，不需要 mock 或录制的 LLM fixture 即可完整测试。
  存储层的每一条性质都能精确断言。
- LLM 编排是快变关注点（模型、prompt、成本策略每周在变）；存储 schema
  是慢变关注点。分离让两者各按各的速度演进。
- Python 已拥有 agent 集成（工具注入、尾提示词撰写、checkpoint 策略）。
  提取是同类工作——属于那里。
- KDL fact 格式是跨界契约。它是 wiki 为原子事实定的权威序列化格式，
  可确定性渲染为 Mermaid/Cytoscape 供检查，Rust 侧解析就是普通的
  `kdl` crate 调用。

## 后果

- mem-core 零 LLM 依赖，入库路径零网络依赖；核心里唯一的网络调用是
  embedding API（ADR-0003）。
- Python 适配层是交付面的一部分（非一次性 demo）：管线编排、KDL 生成、
  融合 LLM 决策、调用方触发策略。
- 未来若出现非 Python 消费方（mem-server），由它重做编排层或对接
  Python sidecar；核心 API 不变。
