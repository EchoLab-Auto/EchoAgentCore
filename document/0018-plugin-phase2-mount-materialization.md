---
id: adr-0018
title: "ADR-0018 插件化 Phase 2：mount 实化"
group: 架构决策
x: 2360
y: 1601
---

# 0018 Plugin Phase 2 — 插件 mount 实化（让禁用/启用有真实效果）

日期: 2026-09-01
状态: proposed

## 问题

ADR-0013 落地了 Phase 1 骨架（crate/类型/注册表/协议/持久化/面板列表），但全部
10 个内置插件的 mount 闭包是空壳（`main.rs` 返回 `Ok(vec![])`），真实装配仍在
组合根手工完成。后果：

- 禁用内置插件（TogglePlugin / persona 白名单）只影响展示与持久化，无任何
  运行效果——QQ 适配器、管理面 WS、工具集在"禁用"后照常运行
- 启动顺序曾是反的（先无条件 mount 再 apply_disabled；本次已修复：禁用插件
  启动时不挂载）
- plugins_dir 数据插件热重载是浅的：无内容 diff 重挂载、删除检测因
  `PluginDescriptor.builtin` 硬编码 true 成为死代码、挂载的数据插件不做真实注册
- `framework_update` 缺 ADR-0013 设计的 `action=plugins`

## 决策

Phase 2 按"效果真实化"分三步推进，每步独立可部署：

### 第 1 步：修语义漏洞（本次已落地）

- 启动顺序：`apply_disabled` 先于挂载；`register_and_mount` 对禁用插件只注册
  不挂载
- update.sh 插件校验补全 10 个 id（原缺 branch.reply/session.global/
  chatbot.sessions）
- 文档与注释对齐实际行为（TogglePlugin、TeamMember.disabled_plugins、
  core-plugins.md 能力开关一节）

### 第 2 步：可安全逆注册的内置插件先实化

按可逆性从易到难，逐个把组合根的装配搬进 mount 闭包：

1. **management.panel**：mount = 起 management WS 监听；unmount = 停止
   （disposer 持 CancellationToken）。无 per-persona 状态，最安全
2. **tools.builtin**：mount = `ToolRegistry::register_reversible` 批量注册；
   注意工具注册表是 per-persona 的——MountContext 的 ToolSink 需扩展为
   遍历 agent_manager 全部 persona 注册/卸载
3. **adapter.qq**：mount = 起适配器（含 QQ 管理工具注册）；unmount = 停止。
   依赖 persona 工具注册表，与第 2 步共用 ToolSink 扩展
4. **skills.dir**：mount = 注册技能目录进各 persona 的 SkillRegistry
   （SkillSink 占位实化）

### 第 3 步：难逆注册的延后

- **provider.llm / loop.runner**：运行中替换 provider/loop 涉及在途 turn，
  保持"重启生效"语义，mount 闭包只登记工厂（禁用 = 下次重启不装配，
  文档明说）
- **orchestration**：编排工具内联于 loop，实化依赖 echo-loop 迁移完成度

### 数据插件（plugins_dir）修复

- 内容 diff：记录 manifest 哈希，变化即 unmount+remount
- `PluginDescriptor.builtin` 按来源正确赋值（修掉删除检测死代码）
- skill/tool kind 的数据插件 mount 做真实注册（复用第 2 步的 sink）

### framework_update

- 补 `action=plugins`：列出/启停插件（复用 TogglePlugin 授权语义），
  与 status 的插件摘要共用 `gather_plugin_summary`

## 备选方案

- **一次性全量实化**：风险大（provider/loop 在途状态难处理），放弃
- **维持 Phase 1 名义挂载**：面板开关持续误导用户，放弃

## 后果

- 第 1 步已修掉"禁用插件仍在启动时挂载"的潜在 bug，并消除文档误导
- 第 2 步后 TogglePlugin 对 4 个插件有真实效果；per-persona 能力白名单
  从 UI 门控扩展到工具/适配器层
- MountContext 需要扩展（AdapterRegistry 锚点、per-persona sink 遍历）——
  echo-plugin crate 的定义面有一次小的演进
