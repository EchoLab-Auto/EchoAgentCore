---
id: panel-system
title: "Panel 系统反馈"
group: 前端
x: 3200
y: 780
---

# Panel 系统反馈

> **定位**：跨视图的系统级反馈契约——Toast 的形态/去重/来源，键盘操作的完整清单（Panel 无全局快捷键系统，所有键处理局部于组件）。读者：改「操作结果怎么告诉用户」「某个键为什么没反应」的前端开发者。相关：[交互定义](./panel-interaction.md)（状态与动作）、[关键常量与设计边界](./panel-constants.md)（Toast 时长、队列上限等数值）。

## Toast 系统

- 三类：`info / success / error`；位置右上；单条 **6s** 自动消失；同屏上限 8 条（`ToastProvider` 的 `max-count`，溢出挤掉最旧）；队列 cap 8、文本截断 512 字符（`App.vue` 的 Toast 桥 + `state.ts` 的 `pushToast`）
- `state.toasts` 仅作转发队列：watcher 逐条泵入组件库 ToastProvider 后清空（`App.vue` 的 Toast 桥）
- 来源与类型：断连提示「与后端断开，正在重连…」（error）、断连时发送命令（error）、Core `Error` 事件（**按 info 展示**，`state.ts` 的 `BackendEvent.Error` 分支——Git 安装等异步操作的失败也经此通道呈现）

## 键盘清单

Panel 无全局快捷键系统；所有键处理局部于组件：

| 键 | 位置 | 行为 |
|---|---|---|
| Enter | 输入区 | 发送消息（IME 组合中不触发） |
| Shift+Enter | 输入区 | 换行 |
| Enter | Shell 命令行 | 执行命令 |
| Esc | Shell 命令行 | 清空输入 |
| Esc | 边栏卡片拖拽中 | 取消拖拽（`RailStack.vue` 的拖拽态 `Escape` 分支） |
| Enter | 设置视图技能触发词输入 | 添加触发词（`SettingsView.vue` 的「触发词就地添加」输入） |

## 反馈形态的选择

同一条信息按「是否需要用户回应」选形态，不混用：

| 形态 | 用在 | 不用于 |
| --- | --- | --- |
| Toast | 异步操作的结果（保存、删除、安装、重连）——用户不需要回应，看一眼即可 | 需要用户抉择的危险操作（用确认框） |
| 确认框 | 不可逆或影响他人（删除、迁移、归档）——见[桌面端模态与覆盖层](./panel-desktop-overlays.md) | 可撤销的小操作（会养成盲点确定的习惯） |
| 就地状态 | 持续存在的状态（加载中、断连、忙碌、未拿到回执）——挂在触发它的位置上 | 一次性结果（会长期占位） |
| 空态文案 | 确认无数据时的占位 | 加载中的等待（那是加载态，不是空态） |

Toast 的时长、队列与截断长度等数值见[关键常量与设计边界](./panel-constants.md)「关键常量速查」。
