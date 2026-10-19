# ADR-0007: 消费边界——唯一跨进程形态（HTTP），无 Python 层

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

消费 k10r 的 agent 已定型（ADR-0006）：gravity，无状态 agent。两条
边界已在别处精确固定，照旧成立：

- **LLM 调用归 gravity**（模型身份绑定到发起推理的一侧）；唯一例外
  是 k10r 内部的 embedding（嵌入空间与数据正确性绑定，归记忆系统
  自持）。
- **aura 世界的公民是 gravity，不是 k10r**（Milestone B Phase 6：
  gravity 作为摊位类型被承载）。k10r 完全站在 aura 之外，与 AI 端
  点同类——外部依赖，契约是 URL 可达。

## Decision

**k10r 是独立网络服务——Fjall 存储、embedding 自持、HTTP API——纯
Rust，无 Python 层。它不是 aura 公民：不注册摊位、不被 aura spawn、
不占 ns。**

1. **HTTP（mem-server）是唯一的跨进程形态。** mem-server 从"预留
   壳"转正为唯一远端面，承载 channel-log 端点（ADR-0008 最小切片
   起步：append / fetch_full / append_checkpoint）。
2. **进程内 mem-core 是本地同机实现细节，不是一种接入方式。**
   gravity Phase 0 直接 link mem-core、进程内调 `ChannelStore`，那是
   一台机器上的一种部署选择，对边界不可见。MemorySurface 缝合层
   （进程内 / HTTP 两个实现）是 gravity 侧的事，不是 k10r 的接入
   方式。
3. **FFI 保留**（mem-ffi，PyO3）：绑定已存在，并继续充当 Phase 0
   绑定冒烟测试。它是消费通道，不是 k10r 的层；其它绑定（或不用绑
   定）同样合法。

早先的"三种接入方式"提法撤销：跨进程边界到达 k10r 的方式不是三
种——恰好一种（HTTP）；进程内 link 与绑定根本不是边界形态。

**k10r 的 API 分两类，不可互换。** **核心 API** 是消费方的操作真正跑在
上面的那些门——会话投影与追加、记忆答案（`recall`）、身份精确入口、调用方
上报的计数、code 存储。它们的**形状就是契约**：改动属于本 ADR 这一级的决定，
消费方的一轮只能走这一类。**分析 API** 是只读检视面——原始图、单独一步落点、
一跳、以及没有路由的核心**读**原语，服务审计、可视化与调试。它次一等：形状
可以随分析需要改变，永远不进模型的工具清单，消费方的一轮也不许依赖它。
这条线画在「结构可以出现在哪里」上：内部结构（node id、edge id、community id）
可以**出现在核心结果里**当句柄，但**维护**那个结构从来不是消费方的活儿——
请消费方去列出、拉取、修补、回写 k10r 自己的索引的门既不属于核心也不属于
分析，它是设计错误（2026-10-20 撤除的那四扇固化门就是它）。两份清单分列在
`docs/SEAM.md`，一表一张。

**两类门各在一个路径前缀下**，于是分类在 URL 上就看得见，不必回表查：
`/memories/{scope}/…` 是核心记忆面（写入、答案、精确身份入口、上报的计数），
`/graph/{scope}/…` 是分析面（整张图、单独的落点、一跳），会话与代码的门仍是
`/channels` 与 `/code`。旧的 `/facts` 前缀一条都不留：改名的门就是断掉的门，留别名
等于让消费方的一轮继续依赖一个分析面可以随手改的形状。

**LLM 编排归 gravity。** 抽取管线的 LLM 决策（分类抽取、out-of-domain
融合判断、策略需要时的摘要）是 gravity 的工作——k10r 只交出确定性
原语和便宜半步（候选集），绑定模型的决策发生在拥有模型身份的一侧。

**embedding 是 k10r 的内部细节，而且它不是一次网络调用。** 模型内置、
进程内运行（ADR-0003）：向量空间绑定数据正确性，所以持有 embedding
一致性的是记忆系统、不是任何调用方，调用方也从不提供向量。写入与查询
路径在进程内计算 embedding。

**密钥绝不进配置文件。** 配置是一份 KDL 文档
（`config/krystallizer.kdl`，每个子系统一个顶层 node）；每个密钥通
过持有它的环境变量名来引用。

## Consequences

- ADR-0004 的归属主张（"抽取在 Python 层"）被取代；其余内容——确
  定性 `ingest(fact)` 元组接口面、融合拆分（核心=便宜半步，调用
  方=昂贵半步）——照旧成立，调用方改为 gravity。
- **aura-actor 接入方式撤销（2026-10-10），连同其 2026-10-09 的 bgi
  修订一并撤销。** k10r 永远不会作为 `language: "induction"` 摊位类型被
  aura spawn 的独立进程，也永远不以 aura 内 actor 身份运行。
  gravity 支持 booth/cli 等载体模式；aura 托管的 k10r 进程没有存在
  必要——aura 侧经 gravity 到达记忆，gravity 经 HTTP 到达 k10r。
  在案的两代被取代形态：最初的 wasm 形态（OKM schema 编译进 wasm，
  存储经 NestStorage 帧上抛归 aura）死于自身的洞——wasm 沙箱既装
  不下文件系统也**没有出网能力**，而 embedding 调用曾是 k10r 唯一自
  持的网络面（ADR-0003：嵌入空间与数据绑定，外移是正确性违规不是
  偏好；ADR-0034 撤销 wasi-http 后，wasm 出网的唯一规划路径关闭）。
  bgi 形态补上了那个洞，却保留了错误的前提：k10r 作为 aura 托管进
  程。
- **aura PLAN Phase 6.6（Storage Booth / NestStorage 执行器）随之出
  局**——k10r 的存储从来不需要 aura 托管执行器。无论调用方是谁，
  k10r 自持 Fjall 目录（ADR-0002）；"磁盘上一个目录"照旧成立，不存
  在承载分流。
- mem-server 是交付物而非预留壳：先落 channel-log 端点（append /
  fetch_full / append_checkpoint），随后是 code 存储，随消费需求增长；flat
  记忆那扇门落地过，又随 flat 表一起撤除（2026-10-19）。
- **核心的出网面是空的。** embedding 模型内置（ADR-0003），所以本文档
  当初让出的那一次调用现在并不存在：无 LLM client、无传输框架、无
  embedding API。边界是严格变窄，不是新增自由。
- 私有配置解析 crate（基于 knus，含 env 合并）在第三个消费需求
  （KDL 形态的 CLI）真实落地后再建；当下 knus 单独覆盖 decode，env
  读取离 `std::env::var` 只有一行。