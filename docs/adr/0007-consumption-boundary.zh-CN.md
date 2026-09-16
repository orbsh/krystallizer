# ADR-0007: 消费边界——三种接入方式，无 Python 层

**Status**: Accepted
**Date**: 2026-09-14
**English**: [0007-consumption-boundary.md](0007-consumption-boundary.md)
**取代**: ADR-0004 的层级归属决定（Python 适配层）

## Context

ADR-0004 把抽取管线的 LLM 编排放进了"Python 层（skillforge 侧，后续
的 mem adapter 包）"。这反映的是 skillforge 恰好怎么实现的，不是本
项目的性质。Krystallizer（下称 k10r）的方向是**去 Python**：它是被
其它项目消费的 Rust 记忆核心；mem-ffi 是其中一种绑定，不是架构的
一层。

与此同时，消费 k10r 的 agent 已定型（ADR-0006）：gravity，无状态
agent。gravity 有两种运行形态——CLI 进程，以及 Aura 内部的 actor。
两种形态到达 k10r 的方式不同，而 wiki 的无状态 Agent 架构页已精确
固定了一条边界：LLM 调用归 gravity（模型身份绑定到发起推理的一
侧）；唯一例外是 k10r 内部的 embedding（嵌入空间与数据正确性绑
定，归记忆系统自持）。

## Decision

**k10r 是纯 Rust 记忆服务，三种接入方式，无 Python 层。**

1. **FFI**（进程内）：mem-ffi（PyO3）——可能的消费者之一，一个绑
   定壳。它是消费通道，不是 k10r 的层；其它绑定（或不用绑定）同样
   合法。
2. **HTTP**（mem-server）：gravity 以 CLI 进程执行时，走 k10r 的
   HTTP 接口。mem-server 从"预留壳"转正为 HTTP 接入方式的承载者。
3. **Aura invoke**（actor 间调用）：gravity 作为 Aura actor 运行
   时，k10r 本身也是 actor，gravity 经 invoke 调用它。

**LLM 编排归 gravity。** 抽取管线的 LLM 决策（分类抽取、out-of-domain
融合判断、策略需要时的摘要）是 gravity 的工作——k10r 只交出确定性
原语和便宜半步（候选集），绑定模型的决策发生在拥有模型身份的一侧。

**embedding 是 k10r 的内部细节**——核心唯一的网络调用（ADR-0003
已让步）。embedding 模型与数据锁定：不同模型的向量不共享空间，所
以持有 embedding 一致性的是记忆系统，不是任何调用方。写入与查询路
径在进程内计算 embedding。

**密钥绝不进配置文件。** 配置是一份 KDL 文档
（`config/krystallizer.kdl`，每个子系统一个顶层 node）；每个密钥通
过持有它的环境变量名来引用。

## Consequences

- ADR-0004 的归属主张（"抽取在 Python 层"）被取代；其余内容——确
  定性 `ingest(fact)` KDL 接口面、融合拆分（核心=便宜半步，调用
  方=昂贵半步）——照旧成立，调用方改为 gravity。
- mem-ffi 不再是交付面；保留为 Phase 0 绑定冒烟测试和一种可用的消
  费方式。
- 核心的网络面恰好一个调用：embedding API。无 LLM client，无传输
  框架。
- 私有配置解析 crate（基于 knus，含 env 合并）在第三个消费需求
  （KDL 形态的 CLI）真实落地后再建；当下 knus 单独覆盖 decode，env
  读取离 `std::env::var` 只有一行。
- **存储承载按运行形态分流。** CLI/独立进程形态：k10r 自持 Fjall
  目录（ADR-0002），"磁盘上一个目录"照旧成立。Aura actor 形态：
  wasm 沙箱内无法自持文件系统，存储承载归 aura——k10r 的 OKM
  schema 代码原样编译进 wasm，`VirtualStorage` 的实现替换为帧上抛
  （okm-wire `OpFrame`/`OpResponse`），host 侧由 NestStorage 执行器
  （aura PLAN Phase 6.6 Storage Actor）接收：prepend registry 分配
  的 app ns 前缀，在 aura 的 okm 实例上执行，回填。schema 语义
  （derive、Table/EdgeTable、WriteBatch 组帧）自持；物理存储与引
  擎归 aura。此形态下 k10r 走静态 OKM derive，**不需要** okm
  dynamic——编译期 schema 是它本来就有的。
