---
id: gen-evidence-04
title: "证据 · 04 仓库竞争"
group: 规划
x: 2240
y: 3800
---

# 仓库竞争状态记录（2026-10-16 12:24 核实）

> 用途：执行阶段调度依据。多会话共享同一物理仓库（core/panel），必须避让他人 WIP。

## Core 仓库（~/.local/share/echo-agent-core/source）
- HEAD 从 3541b46 **前进到 ee9ef08**（12:21:17，install.sh --cn-mirror）——**另一会话正在活跃提交**。
  - 设计文档基线引用 3541b46：行号级引用可能轻微漂移；执行前复核目标文件 HEAD。
  - 本任务产出的 `document/design/adapter-genericization/` 为未跟踪文件，与该会话无文件交集。
- 基线核对：cargo test --workspace 全绿（12:15 前后，ee9ef08 前）。

## Panel 仓库（~/.local/share/echo-agent-panel/source）
- **存在未提交 WIP（疑似另一会话/用户在进行弹层重构）**：
  - `M web/src/components/ChatView.vue`（12:08；-202/+23 行：弹层几何逻辑抽到 PanelPopover）
  - `?? web/src/components/PanelPopover.vue`（12:04；新增通用弹层组件）
  - `M web/src/components/WorkspacePanel.vue`（11:29；仅注释）
- **vue-tsc 基线非全绿**：`ChatView.vue(24,10) TS6133 'checklistKey' declared but never read`（WIP 造成，导入未删）。
  - 执行阶段验收口径：`vue-tsc -b` 允许「恰好此 1 条已知错误」，任何**新增**错误即失败；WIP 落定后此条消失。
- vitest 基线：56 文件 / 418 用例全绿（12:12，WIP 之后）。

## 调度约束
1. **Panel 阶段 1 切片**（02a/02b 范畴）：推荐只动 `protocol.ts / state.ts / pending.ts / QqPanel.vue / QqLoginSection.vue / 对应测试`——与 WIP 文件集（ChatView / WorkspacePanel / PanelPopover）**零交集**，可立即执行。
2. **Panel 阶段 2 切片**（03b 范畴）：涉及 ChatView.vue（挂载点替换）——**必须等 WIP 落定（提交或撤销）后执行**；其文档中的 ChatView:40/839 行号届时复核。
3. **Core 切片**：目标文件（echo-protocol / echo-plugin-host / echo-agent / core-main）与 ee9ef08 的 install.sh 无交集，可执行；执行前 `git log -1` 复核并让执行者只 add 自己改的文件（不 git add -A）。
