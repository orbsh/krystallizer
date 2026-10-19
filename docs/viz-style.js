/* k10r 可视化惯例 —— 类型 → 颜色的单一来源。
 *
 * 谁在用：docs/kg-demo-3d.html 用 <script src> 加载本文件（file:// 下 fetch 本地 JSON
 * 会被 CORS 拦掉，所以约定表是 JS 模块而不是 JSON），页面从它取色、生成图例。
 * 为什么单独存一份：颜色与类型的对应关系是"看图的惯例"，不是某一个页面的细节 ——
 * 放进页面里就要在每个新视图里复制一遍，久了必然漂移。
 *
 * 这套词表描述的是 krystallizer 自己的文档 / 架构图（ADR、文档、crate、表、
 * 接口面、消费方、概念、欠账），不是业务示例：图中每个节点都能在仓库里找到出处。
 *
 * 约定（节点按 kind 取色，边按关系 kind 取色）：
 *
 *   nodeKinds                        edgeKinds
 *   adr      决策记录 #818cf8        declares   声明     #60a5fa
 *   doc      文档     #38bdf8        updates    修订     #a78bfa
 *   crate    代码单元 #2dd4bf        supersedes 取代     #f472b6
 *   table    表 / ns  #a3e635        implements 落地于   #34d399
 *   surface  接口面   #f472b6        consumes   消费     #fbbf24
 *   consumer 消费方   #fbbf24        cites      引用     #94a3b8
 *   concept  领域概念 #c084fc        depends    依赖     #a3e635
 *   owed     欠账/未决 #ef4444       deferred   延后登记 #ef4444
 *   other    其它     #cbd5e1        other      其它     #64748b
 *
 * kind 是机器名（稳定的、可入词汇表的），label 是人看的文字 —— 与 k10r 的建模一致：
 * 种类是运行时数据（名字 ↔ predicate_id 的图谓词词汇表，0x04/0x05），人类可读的名字在 payload 里。
 * 所以本表按 kind 建索引，不按中文标签建索引。
 *
 * 新增一类：在对应表里加一条即可，页面与图例自动跟上；未登记的 kind 落 fallback。
 */
window.K10R_VIZ_STYLE = {
  version: 1,
  nodeKinds: {
    adr:      { color: '#818cf8', label: 'ADR · 决策记录' },
    doc:      { color: '#38bdf8', label: 'Doc · 文档' },
    crate:    { color: '#2dd4bf', label: 'Crate · 代码单元' },
    table:    { color: '#a3e635', label: 'Table · 表 / ns' },
    surface:  { color: '#f472b6', label: 'Surface · 接口面' },
    consumer: { color: '#fbbf24', label: 'Consumer · 消费方' },
    concept:  { color: '#c084fc', label: 'Concept · 领域概念' },
    owed:     { color: '#ef4444', label: 'Owed · 欠账 / 未决' },
    other:    { color: '#cbd5e1', label: 'Other · 其它' }
  },
  edgeKinds: {
    declares:   { color: '#60a5fa', label: '声明 declares' },
    updates:    { color: '#a78bfa', label: '修订 updates' },
    supersedes: { color: '#f472b6', label: '取代 supersedes' },
    implements: { color: '#34d399', label: '落地于 implements' },
    consumes:   { color: '#fbbf24', label: '消费 consumes' },
    cites:      { color: '#94a3b8', label: '引用 cites' },
    depends:    { color: '#a3e635', label: '依赖 depends' },
    deferred:   { color: '#ef4444', label: '延后登记 deferred' },
    other:      { color: '#64748b', label: '其它 other' }
  },
  fallback: { node: '#cbd5e1', edge: '#64748b' }
};