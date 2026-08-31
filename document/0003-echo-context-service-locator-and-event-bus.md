---
id: adr-0003
title: "ADR-0003 服务定位与事件总线"
group: 架构决策
link: ["adr-index | 决策索引"]
x: 616
y: 48
---
# ADR-0003: echo-context — 服务定位、事件总线、可逆注册、作用域

状态: accepted

## 问题

EchoAgentCore 的依赖与通信都走硬编码路径:`Agent` 构造器注入所有服务(provider/
tools/skills/adapters),事件通过封闭的 `BackendEvent` 枚举 + 单消费者 mpsc 单向
广播,注册(工具/监听器)不可撤销,无 per-agent 隔离。这与 dsh 的"一切皆插件、
注册是可逆副作用、类型化事件是扩展点、作用域注册"四条核心模式全部相悖。

## 决策

新建 `echo-context` crate(依赖仅 tokio/dashmap,不依赖任何 echo-* crate),
提供四个机制:

1. **`Ctx` 服务定位**:按字符串 key 注册/解析服务。注册接受 `Arc<dyn Trait>`
   (trait 对象作为具体类型存入 `Box<dyn Any>`,resolve 时 downcast 回同一
   `Arc<dyn Trait>` 类型),也可注册具体 `Clone + 'static` 值。
2. **`Disposer` 可逆注册**:`Ctx::register`/`EventBus::subscribe`/注册表方法都
   返回 `Disposer`,Drop 或显式 `dispose()` 即撤销(移除服务/监听器/条目)。
3. **`EventBus` 类型化事件**:`Event` trait(Any + Clone + Debug + Send + Sync +
   'static),按事件 `TypeId` 分组注册监听器;四种分发模式——`Observe`(扇出)、
   `Waterfall`(around-middleware,`next()` 委托、`ShortCircuit` 短路)、
   `Parallel`(每监听器一份事件副本并发)、`Serial`(按序)。`emit_sync` 覆盖
   同步热路径(Observe/Waterfall),`emit` 支持全部模式。
4. **`ScopedRegistry` 作用域注册**:全局条目 + per-scope 条目,lookup 先 scoped
   后 global(shadowing),scope 卸载时条目随 disposer 撤销。

接入:组合根(main.rs)用 `Ctx` 注册 `ctx.llm`;`Agent` 持有 `event_bus` 并在
`emit` 中广播;display timeline 投影(`TimelineProjector`,新模块 timeline.rs)
从 `Agent::emit` 剥离,改为 EventBus 的 observe 监听器,删除 Agent 上的
timeline 状态字段与方法;`ToolRegistry` 支持 `register_reversible`(返回
Disposer,失效 definitions 缓存)。

## 备选方案

- **不引入 Ctx,维持构造器注入**:服务依赖仍是编译期图,无法运行期替换。
- **Ctx 用 TypeId 而非字符串 key**:dsh 用 `ctx.<key>` 字符串键;字符串便于
  配置与诊断,且允许同一 trait 多实例按名共存。
- **事件用 trait object 广播而非类型化**:丢失编译期类型安全与 per-type 分发。
- **Parallel 事件要求 Clone**:并行分发每监听器一份副本是安全前提;要求
  `Event: Clone` 是明确契约而非隐式行为。

## 后果

- 依赖方向:`echo-context` 是最底层机制(仅 tokio/dashmap);`echo-protocol`
  为 `BackendEvent` 实现 `Event`(克隆/调试/发送/同步已派生,此 impl 声明成员
  资格)——协议 crate 由此依赖 context,方向仍为 context ◄ protocol。
- `Agent` 不再直接写 timeline:timeline 是事件的投影消费者,可独立演进/测试;
  前端桥接未来同样迁移为总线监听器。
- 注册必须持有 disposer 才生效(组合根持有 `ctx` 注册、Agent 持有 timeline
  订阅)——这符合"注册是可逆副作用"语义,但要求装配方明确管理生命周期。
- 行为零变化:431 项测试全绿;`Agent::emit` 对外语义(先 timeline 后前端)不变。
