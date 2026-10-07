---
id: panel-interaction
title: "Panel 布局 & 交互定义"
group: 前端
x: 2720
y: 0
link: ["panel-layout | 布局与导航 | b>t", "panel-chat | 会话视图 | b>t", "panel-modals | 模态与覆盖层 | b>t", "panel-settings | 设置视图 | b>t", "panel-qq-tasks | QQ管理·任务·Shell | b>t", "panel-system | 系统交互 | b>t"]
---

# Panel 布局 & 交互定义

Panel 前端全部交互行为的**总入口**：总览（层级/树状）、核心原则，以及各模块交互详情的引申节点。每个模块节点描述一个子系统的交互与布局设计；实现位置以 `web/src/` 相对路径标注；渲染基元来自 `@echolab-auto/ui-frame`。

**核心原则**：

1. **Core 是状态的唯一事实来源**——前端不持有业务真相，一切展示由 WS 事件流驱动（本地时间线缓存只是加速副本，2026-09 刷新加速）；断连重连后按缓存游标增量补齐、缓存失效时由 Core 回退全量；唯一例外是**取消任务的乐观中断**（先本地把运行中态置为已取消，再由 Core 事件确认）
2. **实时优先**——推理/工具/回答按事件到达顺序即时渲染，不缓冲等待（规范见 [Panel 前端](./panel.md)「时序与动画规范」）
3. **危险操作显式确认**——删除/清理走确认（两步确认或原生 confirm）；升级类操作不可静默执行
4. **编辑即生效**——QQ 门控、启停开关等操作无草稿态，点击立即下发命令
5. **加载 / 空 / 错误三态区分**（2026-09-30）——「（暂无…）」空态只在**确认无数据**
   时展示；请求等待期由 `pending.ts` 统一跟踪（`sendPending` 登记请求→响应事件
   销账→150ms 延迟显示 / 6s 慢提示 / 20s 超时重试，详见 [Panel 前端](./panel.md)
   「状态管理与数据流」），各面板以 `LoadHint` 呈现等待与错误；断连（最高优先态）
   由视图判断 `state.connected`，优先于加载态

## 交互模块树

```prodoc-flow
graph TD
  Root[Panel 布局 & 交互定义|/panel-interaction.md] --> L[布局与导航|/panel-layout.md]
  Root --> C[会话视图|/panel-chat.md]
  Root --> M[模态与覆盖层|/panel-modals.md]
  Root --> Set[设置视图|/panel-settings.md]
  Root --> QT[QQ管理·任务·Shell|/panel-qq-tasks.md]
  Root --> Sys[系统交互|/panel-system.md]
  L --> L1[视图层级与导航]
  L --> L2[应用外壳布局]
  L --> L3[侧边栏]
  L --> L4[Agent 切换器]
  L --> L5[连接生命周期]
  C --> C1[消息列表与角色]
  C --> C2[工具卡与推理块]
  C --> C3[输入区]
  C --> C4[入口行与弹出层]
  M --> M2[分支详情]
  M --> M3[上下文弹层]
  M --> M4[Agent 配置弹层]
  Set --> S1[API 设置]
  Set --> S2[资源工作区]
  Set --> S3[Git 安装技能]
  Set --> S4[日志]
  QT --> Q1[QQ 管理]
  QT --> Q2[任务弹层]
  QT --> Q3[Shell 列表/详情]
  Sys --> Y1[Toast]
  Sys --> Y2[键盘]
  Sys --> Y3[常量速查]
  Sys --> Y4[设计边界]
```

## 模块索引

| 节点 | 内容 | 对应章节 |
| --- | --- | --- |
| [布局与导航](./panel-layout.md) | 视图层级、外壳几何、侧边栏、Agent 切换器、连接生命周期 | 原 §一~§六 |
| [会话视图](./panel-chat.md) | 消息列表、工具卡、推理块、输入区、入口行 | 原 §七 |
| [模态与覆盖层](./panel-modals.md) | 分支/上下文/Agent 配置、确认形式 | 原 §八 |
| [设置视图](./panel-settings.md) | API/资源工作区/Git 安装/日志 | 原 §九 |
| [QQ 管理·任务·Shell](./panel-qq-tasks.md) | QQ 管理、任务弹层（入口行）、Shell 列表（边栏卡）与详情视图 | 原 §十~§十一 |
| [系统交互](./panel-system.md) | Toast、键盘、设计边界、常量 | 原 §十二~§十五 |

> 2026-09-04 起原「资源」「日志」视图与 API 设置弹窗合并为「设置」视图（见 [设置视图](./panel-settings.md)）；本节点为总入口，子节点间不互相 link（仅经本节点导航）。
