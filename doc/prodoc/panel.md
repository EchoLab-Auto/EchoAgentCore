---
id: panel
title: "Panel 前端"
x: 760
y: 40
group: 架构
link: ["protocol | 协议与数据流"]
---

# Panel 前端

Panel（EchoAgentPanel）是 Web 管理面板：Vue 3 + TypeScript，基于 `@echolab-auto/ui-frame` 新拟态组件库。源码在仓库 `web/src/`，构建产物由 Rust 服务托管（`echo-agent-panel-bin`，默认 `:8080`，前端 `/ws` 连 Core 的 management WS `:3132`）。

## 状态管理与数据流

- `store.ts`：全局响应式单例（Vue `reactive`），`dispatch(event)` 逐事件归约
- `state.ts`：reducer 按事件类型分派（约 30 个分支），原地深变异
- `state_domains/`：timeline（时间线转换/增量）、orchestration（分支/任务/活动）、helpers
- 连接管理 `connection.ts`：WS 自动重连（指数退避），重连后清空运行期状态并 Bootstrap

## 主视图（聊天）

- `ChatView.vue`：消息列表 + 吸底输入区；`#message` slot 拦截扩展角色渲染
- `ReasoningBlock.vue`：推理打字机动画（实时消息 6 秒封顶；历史回放不播）
- 活动浮条：思考中 / 调用工具 / 子代理 的 spinner + 动态文案
- 消息入场动画 0.28s 淡入上移，仅实时消息（`animate` 标记）播放

## 侧边栏与资源页

- `PanelSidebar.vue`：临时分支卡 + 会话卡（按当前 agent 能力开关显示/隐藏）
- `SessionGroups.vue`：按平台分组（全局/Local/QQ 私聊/QQ 群/其他），按 agent 过滤
- `CapabilitiesPanel.vue`：资源中心——技能/工具/插件/Team 的浏览、启停、编辑、删除
- `AgentSwitcher.vue`：输入框上方 Agent 切换悬浮卡片

## 时序与动画规范

推理、工具调用、正式回答**按事件到达顺序实时插入**主时间线，推理真实穿插在工具调用之间；正式回答始终最后。规范见 Panel 仓库 `doc/decisions/0004-panel-agent-visual-timeline.md`。

## 主题

- `ThemeProvider` + `NeumorphismThemeToggle` 三态开关（浅色/自动跟随系统/深色）
- 偏好持久化于 localStorage（`echo-panel-theme`），index.html 防闪烁脚本同步初始化

## 配置与运维

- `~/.config/echo-agent-panel/panel.toml`：`[server]`（bind_address、static_dir）、`[core].connect_url`（连 Core 的 WS 地址）
- `systemctl --user restart echo-agent-panel.service` 重启；静态资源在 `~/.local/libexec/echo-agent-panel/web`
- 更新走 `echo-agent-panel-update.service`（构建 Rust 服务 + `npm run build` + 替换静态目录 + 重启）
