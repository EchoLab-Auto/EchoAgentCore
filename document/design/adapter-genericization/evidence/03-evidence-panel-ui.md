---
id: gen-evidence-03
title: "证据 · 03 Panel UI"
group: 规划
x: 1920
y: 3800
---

# 证据包：03（Panel 阶段 2：UI 描述符 + schema 渲染器 + 迁移删除）

> 配套：`00-frozen-interfaces.md`。行号基于 panel @ 4b7d99b / core 设计文档 adapter-plugin.md。

## A. UI 描述符规范（v1，设计基线）

来源：core 仓库 `document/design/adapter-plugin.md` §Panel UI 解耦（:274-325）。要点：

```jsonc
{ "version": 1, "panels": [{ "id": "main", "title": "QQ 适配器", "nodes": [
  { "type": "status",  "bind": "online", "label": "连接" },
  { "type": "text",    "bind": "self_id", "label": "账号" },
  { "type": "link",    "bind": "webui_url", "label": "打开管理页" },
  { "type": "select",  "bind": "gate.mode", "options": ["none","allowlist","denylist"],
    "action": { "method": "set_gate_mode", "arg": "$value" } },
  { "type": "list_editor", "bind": "gate.allowlist_users", "label": "用户白名单",
    "action": { "method": "update_allowlist" } },
  { "type": "image",   "label": "登录二维码", "fetch": { "method": "login_qrcode", "result_key": "png_base64" } },
  { "type": "button",  "label": "刷新", "action": { "method": "refresh" } }
]}]}
```

- 节点类型 v1：`status / text / link / select / list_editor / image / button`；**未知节点忽略**；version 分支渲染。
- 状态流：插件 `report_ui_state` → Core 缓存 → `AdapterState` 事件 → Panel `adapterUiByKey[scopedKey(core,adapter)]`。
- 命令流：渲染器 → `InvokeAdapterOp` → Core → 插件 `custom`；结果 `AdapterOpResult`（配对方案见 02）。
- **状态文档路径标准**（bind 引用）：见 00 文件 §三（`online` / `self_id` / `instance.*` / `gate.*` / `owner`）。
- 通用面板上移：多实例切换 / 状态行 / **启停重启按钮**（已中立命令）为 Panel 内置；插件只贡献 schema。
- 会话分组：`SessionSwitcher` 按 platform + `session_labels: {dm,group}` 通用化。

## B. 现状 UI 细节（迁移/parity 对照）

### QqPanel.vue（613 行；含样式）
- 结构：头部（"适配器管理" 标题 + 刷新 :88-101）→ 多实例选择器（:205-217，仅多实例时）→ 无实例空态（:218）→ 适配器状态行（:224-257，含 Start/Stop :240、Restart :248）→ 门控模式（:258-270，GATE_OPTIONS :72-76 / GATE_HINTS :78-82）→ 四名单卡（:271-416：白名单用户 / 黑名单用户 / 白名单群 / 黑名单群；toggle 函数 :137-183）
- 状态读取：`instanceState`（:53）、`activeAdapter`（:46）、`multiInstance`（:60）；数据来自 `state.qqByAdapter[scopedKey(core, name)]`
- pending：`qq:filter|groups|friends|owner:<inst>`（:106-110）；`qqListsPhase`（:110+）
- 测试：`web/src/components/__tests__/QqPanel.test.ts`（226 行）

### QqLoginSection.vue（148 行）
- 登录态 + 二维码（`qrDataUrl` :24）、`loadQr()` :44（RequestQqQrcode）、`requestStatus()` :38、`webuiUrl` :108；`instanceState` 从 `qqByAdapter` 读
- 注意：二维码结果现在全局 `state.lastQrcode` 单槽（见 02 证据 §A.2）

### 其他挂点
- `ConnectionStatusCard.vue:69-77`：`state.adapters` 通用行（label / selfId / stateText / dot）——**已通用**，阶段 2 基本不动
- `SessionSwitcher.vue:56-72`：qq 分支 + GROUP_LABEL（`qq-dm / qq-group`），order（:76）——通用化对象
- `ChatView.vue:40,839`：QqPanel 挂载点（删除时的清理点）

### ui-frame 组件库（web/vendor/ui-frame，可直接复用）
NeumorphismButton / Select / Input / InputNumber / Checkbox / Card / List / StatusDot / Empty / Badge / Alert / Collapse / Form / Field 等（`web/vendor/ui-frame/dist/components/` 全清单可 ls 查看）

## C. parity 清单（验收对照，来源 adapter-plugin.md:326-333）

| 现功能 | 目标归属 |
|---|---|
| 多实例切换 / 状态行 / 启停重启 | Panel 内置通用面板 |
| 门控模式选择 | 插件 schema（select + action） |
| 白/黑名单四卡片 | 插件 schema（list_editor ×4） |
| 登录状态 + 二维码 PNG | 插件 schema（status + image + fetch） |
| owner 设置 | 插件 schema（text/input + action） |
| 会话列表 QQ 分组 | platform 字符串分组 + session_labels |

## D. 交付物要求

- **03a（渲染器规范）**：组件设计（建议 `web/src/components/adapter-ui/` 下；props/emits/文件划分）、7 节点语义表（数据源/交互/action 形状/$value/加载与错误态）、版本兼容、与 `adapterUiByKey` + `InvokeAdapterOp` 的接线（配对细节引用 02，不重设计）、测试规范（节点逐类 + 未知忽略 + action 载荷）
- **03b（迁移与删除）**：逐 UI 元素迁移映射、分阶段序列（依赖 02 阶段 1 完成 + 01 描述符上线）、QqPanel/QqLoginSection/qqByAdapter/QQ 协议段/相关测试的删除与迁移清单、parity 逐项验收、风险与回滚
- 两份文档都：≤400 行、file:line 引用替代大段代码、切片化（可独立提交单元）
