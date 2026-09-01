---
id: adr-0008
title: "ADR-0008 命令分发拆分"
group: 架构决策
x: 2360
y: 720
---
# ADR-0008: 命令分发拆分与 CommandRegistry

状态: accepted

## 问题

`apply_command`(agent/commands.rs,478 行)是 25+ 分支的巨型 match,QQ 专属
命令(名单/门控/群列表/好友列表)与核心命令(模型/API/技能/状态)混在一个
函数里。新增平台命令必须编辑主分发器;QQ 逻辑与 agent 核心耦合。

## 决策

1. **QQ 命令域独立**:`apply_qq_command`(新文件 `agent/qq_commands.rs`,
   第二个 `impl Agent` 块)承载 6 个 QQ 变体
   (`UpdateQqAllowlist`/`UpdateQqDenylist`/`SetQqGateMode`/
   `RequestQqFilterConfig`/`RequestGroupList`/`RequestFriendList`);主
   `apply_command` 的对应分支改为一行委托,其余核心分支原地保留。
2. **`CommandRegistry`**(`command.rs`):`CommandHandler` trait(dyn-compatible,
   async 返回 `Pin<Box<dyn Future>>`)+ 按序注册表——开放命令扩展点的基础
   设施(新增命令 = 注册 handler,编辑分发器的需要随协议开放而消除)。封闭
   枚举 `BackendCommand` 下,编译器 match 完备性检查仍由主分发器承担。

## 备选方案

- **不拆分,维持巨型 match**:QQ 与核心耦合,新增平台命令改主分发器。
- **完整注册表化全部命令**:封闭枚举下注册表收益有限(编译器已保证完备);
  注册表保留为开放扩展载体,现有命令按域拆分已达成结构目标。

## 后果

- QQ 命令逻辑独立成文件,可独立测试/演进;主分发器瘦身为委托 + 核心分支。
- `CommandRegistry`/`CommandHandler` 就绪,协议开放(Phase 5 与 Panel 捆绑的
  协议泛化)后可直接承载外部命令插件。
- 行为零变化:465 项测试全绿;QQ 命令语义与事件原样保留。
- 后续:QQ 适配器迁入 `echo-qq` provider 时,`apply_qq_command` 随之迁移。
