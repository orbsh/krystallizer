# Krystallizer — Agent 记忆核心

> **语言：** [English](README.md)（主文档） · 中文

图谱原生（graph-native）的 agent 记忆：会话控制、原子事实图、向量着陆检索——一个 Rust 核心，由一个可运行的进程（`krystallizer`）经 HTTP 提供，或作为 crate 在进程内链接。

设计文档：[krystallizer.md](https://github.com/orbsh/wiki/blob/main/krystallizer.md)（wiki；English: [krystallizer-en.md](https://github.com/orbsh/wiki/blob/main/krystallizer-en.md)），裁决记录在 `docs/adr/`。

## 它是什么

从 skillforge 的 `src/mem` 独立出来的记忆系统，重建在 [KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md) 与 [图谱化记忆](https://github.com/orbsh/wiki/blob/main/graph-memory.md) 描述的 KV 架构上：

- **会话控制面** —— 把完整的会话控制拆成原语：`branch`（从任意前缀点分叉；
  checkpoint 共享让分叉零拷贝）、`tail`（单轮分支：注入一个 prompt，该轮
  结束即丢弃）、`summarize`（注入 + 截断 + checkpoint）。触发策略归调用方，
  核心只提供原语。
- **记忆面** —— 一项常设能力，以工具形式提供，分三类（ADR-0001：被动 / 短期，
  即循环自己的上下文——channel 日志及其投影；主动 / 长期，
  `store` / `search` / `forget` 作用于事实图——flat memory 是它的前图谱形态，
  已撤销；以及初始化记忆（人设 / 风格）——图派生，注入上下文头部）。
- **任务面** —— 任务也是图：任务顶点 + `task_links` 依赖边，与事实共用同一张
  图；实例态是结构化的（精确枚举），随投影下发。第三个接口面（ADR-0001）。
- **原子事实图** —— append-first 的事实，定长 8 槽元组（角色 `head` / `rel` /
  `tail`——head/tail 为 `[值, 类型, 属性]`，关系为 `[关系名, 属性]`）；写侧的
  collections 与 `ingest(fact)` 于 2026-10-17 落地（实体 ns 36 + 事实 ns 37，
  ADR-0010）。实体锚定 / 时间线 / 因果链三种聚簇作为视图，权重系统（读写
  四象限、`tool_invoke_count`），写入时融合（域内 id 查表 / 域外 embedding
  收敛）。
- **内置检索** —— 一个小型本地 embedding 模型在进程内运行（无 API、无 key；
  向量空间与数据一同锁定）。查询着陆，答案是着陆到的**社区**（它的节点与边，
  按权重排序，在 fact budget 内渲染成带 id 的文本，表示形态由 `?format=`
  指定——`plaintext` / `markdown` / `mermaid`，或 `struct` 只给数据），精确的
  名字/别名点查是第二个入口。没有词法索引、没有融合阶段、没有摘要那一遍、
  没有第二个存储引擎。

## Crates

| Crate | 作用 |
|---|---|
| `mem-core` | 存储 + 检索核心。零 LLM 依赖，零传输依赖。 |
| `mem-server` | axum HTTP 面——唯一的跨进程形态（ADR-0007 修订后）。它提供两类门，在 `docs/SEAM.md` 里分开记：消费方赖以运行的 CORE API（会话投影与追加、`POST /memories/{scope}`、`GET /memories/{scope}/recall`、精确名字入口、`.../edges/{id}/touch`、code store），以及用于审计与可视化的 ANALYSIS API（渲染图 `GET /graph/{scope}`、仅着陆、一跳）。它还持有项目中唯一的出站客户端：`mode "remote"`（ADR-0003）的 embeddings 客户端——这正是 `mem-core` 能保持无传输依赖的原因。 |

存储：Fjall，schema 经 OKM derive 宏声明（编译期 key 编码、hex 稳定性测试）。

## 运行

```
cargo build --release
./target/release/krystallizer [--config <path>] [--addr 127.0.0.1:7760]
```

服务的参数就是一个配置文件 `krystallizer.kdl`：`storage.data-dir`（数据库目录，
不存在则创建，允许 `$HOME`）、`embedding` 引擎——进程内的内置模型
（`mode "local"`），或另一台机器上 OpenAI 形状的 embeddings 服务
（`mode "remote"`：`base-url` / `model` / `api-key-env` / `timeout-ms`）、
`compression` 的数值与 `logs` 上限。`--config` 指定该文件；不给时二进制读
**它旁边**那一份，所以一次部署把两者一起发（本仓的 `config/krystallizer.kdl`
复制到二进制旁边）。缺配置是启动失败，绝不回退：一个「在碰巧所在的目录里新建
数据库」的服务，下次换个地方启动就丢数据。它打印的是**实际绑定**的地址，所以
`--addr 127.0.0.1:0`（由 OS 挑端口）仍然能告诉 supervisor 该往哪看。

两种 embedding 模式在启动时都有验证：缺 artifact、服务不可达、或宽度与
`dimensions` 不一致，进程就停下——因为用不工作的引擎写进去的行，是没人能检索
到的行。

## 状态

设计阶段——相位见 `docs/PLAN.md`，设计见 `docs/adr/`，schema 见
`docs/MODELING.md`，消费方对各接口的操作见 `docs/SEAM.md`，活着的主动添加
工具定义见 `docs/PROMPTS.md`（唯一的 prompt 面；写侧的胶囊生成器随 ADR-0010 的
合并那一遍一并退役）。