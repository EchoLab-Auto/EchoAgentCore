# EchoAgentPanel 旧版本兼容技术债审计（只读）

- 扫描范围：`source/echo-web-server/**/*.rs`、`web/src/**/*.ts|*.vue`、`config/`、`README.md`、`scripts/`、`packaging/`、`Dockerfile`
- 政策口径：仅保留「当前功能健壮性」（重连/乱序/竞态）与浏览器 localStorage 历史数据（DATA）两类例外；
  为旧版 Core / 旧版前端 / 旧格式事件 / 旧配置形态保留的兼容代码 = 待清理。
- 分类：`[REMOVE]` 明确旧版本兼容、可直接删；`[GRAY]` 兼服务当前单上游部署或缺失字段容错、需决策（不确定项加 `?`）；`[DATA]` 浏览器 localStorage 历史数据/迁移；`[DOC]` 文档/注释/文案，仅需改文字。
- 复核方式：grep 关键词（旧/legacy/兼容/已移除/缺省/过渡期）+ 逐处读上下文（proxy.rs 壳路由、config.rs 物化、state.ts reducer、protocol.ts 可选字段）。

## Rust：配置与入口

- source/echo-web-server/src/config.rs:7-8 — 模块注释：旧配置遗留 `[qq]` 段被 serde 静默忽略、加载不报错（向后兼容承诺） — [REMOVE]
- source/echo-web-server/src/config.rs:132-146 — `PanelConfig::upstreams()`：`cores` 键未出现时把 `[core]` 物化为单条上游 "core" — [GRAY]
- source/echo-web-server/src/config.rs:155-165 — `cores_touched` 逐行文本扫描（只为 `[core]` 兜底服务；含 `[[ cores ]]` 空格变体容忍） — [GRAY]
- source/echo-web-server/src/config.rs:189-248 — `cores_touched` 三个测试（守护 `[core]` 兜底与注释示例不误命中） — [GRAY]
- source/echo-web-server/src/config.rs:283-304 — 测试 `legacy_qq_section_is_ignored`：明确要求旧 `[qq]` 段加载不报错 — [REMOVE]
- source/echo-web-server/src/main.rs:31-34,60-75,109-115 — CLI `--core` 覆盖全部配置（单上游、不写回 `[[cores]]` 的临时语义；仓库脚本/文档均未使用） — [GRAY?]
- source/echo-web-server/src/main.rs:76-99 — 非 `--core` 路径的上游物化校验/重名去重（`[core]` 与 `[[cores]]` 两形态共用） — [GRAY]

## Rust：中继（proxy）

- source/echo-web-server/src/proxy.rs:338-340 — 注释「无壳（单上游旧前端）→ 广播全部上游」+ 提及已删除的只读白名单；路由本体是当前单上游语义，措辞过时 — [GRAY]
- source/echo-web-server/src/proxy.rs:349-350 — 注释：旧的"按命令名猜只读"白名单已删除（历史说明） — [DOC]
- source/echo-web-server/src/proxy.rs:351-355,752-783 — `CommandRoute::Unwrapped` 分支：无壳命令仅单上游放行、多上游拒绝（当前单上游部署的常规路径） — [GRAY]
- source/echo-web-server/src/proxy.rs:719-722 — 注释叙事「旧实现静默丢弃…（与旧行为一致）」；代码为建链竞态健壮性 — [DOC]
- source/echo-web-server/src/proxy.rs:756-760 — 同上（无壳命令路径的旧实现叙事） — [DOC]
- source/echo-web-server/src/proxy.rs:896-902 — 测试 `unwrap_plain_command_is_unwrapped`（无壳命令兜底解析） — [GRAY]
- source/echo-web-server/tests/proxy.rs:273,344,394 — 测试文档注释叙述「旧实现静默丢弃」（广播补投/有界等待的动机说明） — [DOC]
- source/echo-web-server/tests/proxy.rs:343-392 — `unwrapped_command_waits_for_link_and_is_delivered`（单上游无壳命令的链路就绪等待） — [GRAY]

## 配置样例与 README

- config/echo-agent-panel.toml:21-25 — `[core]` 单上游段（与 `[[cores]]` 二选一的兜底形态） — [GRAY]
- config/echo-agent-panel.toml:27-30 — 旧 `[qq]` 段"已移除、会被静默忽略、无需手工清理"说明 — [DOC]
- config/echo-agent-panel.toml:32-40 — `[[cores]]` 说明中「单上游（`[core]` 或一条 `[[cores]]`）行为与此前完全一致（无壳）」 — [DOC]
- README.md:23-24 — 「单上游（`[core]` 或一条 `[[cores]]`）行为与此前完全一致（无壳）」 — [DOC]
- README.md:127-129 — `[core] connect_url` 单上游兜底说明 + 旧 `[qq]` 段静默忽略说明 — [DOC]

## 前端：protocol.ts 可选字段与常量

- web/src/protocol.ts:193-196 — `TeamInfo.orchestration_mode`「旧字段（过渡期兼容）：parallel 记为 'chatbot'」 — [REMOVE]
- web/src/protocol.ts:201-206 — `loopModeOf`：旧字段 `chatbot → parallel` 兜底 — [REMOVE]
- web/src/protocol.ts:211-219 — `LEGACY_MODE_PLUGIN_IDS`（旧编排/驱动插件 id，保存防御用） — [REMOVE]
- web/src/protocol.ts:336-337 — `TimelineTool.tool_call_id`「旧 core 无此字段，回退按名配对」 — [REMOVE]
- web/src/protocol.ts:340-343 — `started_at_ms` / `elapsed_ms`「新 core 提供」——旧 core 缺失 — [REMOVE]
- web/src/protocol.ts:355-356 — `TimelineMessage.seq` 可选「新 core 提供」 — [REMOVE]
- web/src/protocol.ts:599-600 — `ContextSnapshot.session_id`「旧 Core 缺省」 — [GRAY?]
- web/src/protocol.ts:607 — `TrunkTimeline` 的 `seq?` / `full?` 可选（`full` 缺省时前端走旧启发式） — [REMOVE]
- web/src/protocol.ts:658-661 — `ApiBalanceResult.granted` / `topped_up`「旧 Core 可能缺省」 — [REMOVE]

## 前端：循环模式旧插件 id

- web/src/loop-mode.ts:13,18-22 — `PARALLEL_IDS` 含 `LEGACY_MODE_PLUGIN_IDS`（任一旧 id → parallel） — [REMOVE]
- web/src/loop-mode.ts:30,53-55 — `stripLegacyModeIds` 保存防御（注释：core 也会迁移映射） — [REMOVE]
- web/src/__tests__/loop-mode.test.ts:18-21 — 测试：任一 legacy 模式 id 推导为 parallel — [REMOVE]
- web/src/__tests__/loop-mode.test.ts:47-49 — 测试：`stripLegacyModeIds` 剔除 retired ids — [REMOVE]
- web/src/components/SettingsView.vue:1247 — 保存团队前 `stripLegacyModeIds` — [REMOVE]
- web/src/components/AgentConfigModal.vue:269 — 保存团队前 `stripLegacyModeIds` — [REMOVE]

## 前端：state.ts reducer / 状态

- web/src/state.ts:82-84,120-128 — `belongsToActiveTeam`：team_id 缺省与 core==null 不过滤（旧 core/单上游；"TeamsList 未到达"属当前健壮性） — [GRAY]
- web/src/state.ts:134-137 — `evtCore` 注释「旧 core / 单上游 → null」 — [GRAY]
- web/src/state.ts:145-152 — `managementCoreCurrent`：core==null 放行（单上游 / 未指定 activeRegion 的当前流程） — [GRAY]
- web/src/state.ts:333-342 — `checklistsForSession`：新键 → 旧键 `(core, session)` 回退 — [REMOVE]
- web/src/state.ts:362-364 — `ContextSnapshotData.session_id`「旧 Core 缺省为 undefined」 — [GRAY?]
- web/src/state.ts:387-390 — `RegionInfo.id`「旧 Core 缺省退化为上游名」（回退实现在 store.ts `learnRegion`） — [GRAY?]
- web/src/state.ts:408,416 — `federations` / `federationInvites` 的 `''` 桶 = 单上游/未加壳事件 — [GRAY]
- web/src/state.ts:429,549,574 — 注释记录已删除的死状态字段（qqFilter / pendingReasoning / qqGateMode 等） — [DOC]
- web/src/state.ts:484-487 — `apiBalances.granted` / `topped_up` 可选（旧 Core 可能缺省） — [REMOVE]
- web/src/state.ts:527-533 — `contexts` 注释：`session_id` 缺失（旧 Core 全局口径）键为 `sessionKey(core,'')` — [GRAY?]
- web/src/state.ts:946-952 — `ChecklistUpdated` 键：team_id 缺失退化为 `(core, session_id)`「与既有键兼容」 — [REMOVE]
- web/src/state.ts:962-968 — `ContextSnapshot` 归键：`session_id` 缺失归入 `sessionKey(core,'')` — [GRAY?]
- web/src/state.ts:974-977 — `TrunkTimeline`：core 未标注（单上游/旧 core）时不过滤 — [GRAY]
- web/src/state.ts:985-989 — `TrunkTimeline` 归属键：team_id 缺失按当前视图处理（旧 core 路由） — [REMOVE]
- web/src/state.ts:992-996 — 旧 core 空增量仅游标对齐 + `full` 缺省回退启发式 — [REMOVE]
- web/src/state.ts:1408-1412 — `FriendList` 注释：此前只写已废弃的全局 qqFriends 字段 — [DOC]

## 前端：keys 与组件

- web/src/keys.ts:5-6,11-14 — `scopedKey`：core 为空退化为裸 id（单上游；同时决定 trunk-cache 磁盘键） — [GRAY]
- web/src/keys.ts:21-30 — `checklistKey`：team_id 为 null（旧 core 未标注）退化为 `(core, session_id)` — [REMOVE]
- web/src/components/ChatView.vue:252-256 — 清单读取注释「team 维度隔离 + 旧 core 回退」 — [REMOVE]
- web/src/components/ChatView.vue:452-457 — 禁用清单时同时删「旧键退化形态」 — [REMOVE]
- web/src/components/ChecklistSidebar.vue:23-26 — 注释「team 维度隔离 + 旧 core 回退」 — [REMOVE]
- web/src/components/ChatView.vue:755-757,1025-1026 — 图片渲染注释「或遗留的 data URI」（无专门分支，仅注释/测试） — [GRAY?]
- web/src/components/__tests__/ChatViewImages.test.ts:84-90 — 测试：遗留 data URI 仍可渲染（老会话兼容） — [GRAY?]
- web/src/components/QqPanel.vue:35-47 — 「兼容旧 Core（无 persona 元数据）」时退化展示无归属实例 — [REMOVE]
- web/src/components/QqPanel.vue:28,98 — 注释「单实例时行为与旧版一致」（措辞，非逻辑） — [DOC]
- web/src/components/ShellList.vue:21-26 — 会话过滤容忍「旧 core 无 team_id」（同文件头声明 team_id 必填，自相矛盾） — [REMOVE]
- web/src/components/ApiSettings.vue:287-296 — 旧配置 provider（anthropic/ollama）保留为额外下拉项 — [REMOVE]
- web/src/components/ApiSettings.vue:420-428 — 余额分账：`granted`/`topped_up` 缺省回落指标快照（快照回落=当前功能）；仅「undefined 即隐藏」分支属旧 Core 缺省兜底 — [GRAY?]
- web/src/components/ContextView.vue:92-99 — 注释「取不到再回退旧 Core 的全局口径」与实际单键查询实现不符（过时注释） — [DOC]
- web/src/state_domains/timeline.ts:133-135,141-143 — `toolCallId` 为空（旧 core）时按名字匹配 running 条目 — [REMOVE]
- web/src/state_domains/timeline.ts:147-153 — `elapsedMs` 缺失按 `startedAtMs` 回算（注释：旧 core 兜底） — [REMOVE]
- web/src/state_domains/timeline.ts:199-201 — 旧数据 backend 条目携带 reasoning 尾巴的拆分 — [GRAY?]
- web/src/state_domains/timeline.ts:17,170 — 注释：旧版双计数器 key 冲突 / 旧版误恢复为 succeeded — [DOC]
- web/src/components/ChatView.vue:582-586 — subagent 输入兼容原始 JSON「其他来源/旧数据」 — [GRAY?]
- web/src/connection.ts:134-136 — 注释「单上游时中继不加壳，parseEvent 直接命中」 — [GRAY]
- web/src/connection.ts:228-238 — `parseFrame`：无壳 = 单上游原格式解析 — [GRAY]
- web/src/connection.ts:319-329 — `sendCommand`：core 解析为空时无壳直发（单上游路径） — [GRAY]
- web/src/store.ts:63-66 — dispatch 注入注释「单上游/旧 core 不注入」 — [GRAY]
- web/src/components/FederationSettings.vue:38-46 — 单上游/空名 → 无壳直发 — [GRAY]
- web/src/components/FederationSettings.vue:56-70 — 单上游回落"唯一桶"取联邦状态/邀请串 — [GRAY]
- web/src/workspace.ts:75-90 — `workspacePickIntent` 的 `knownWorkspaceIds` 可选参数（测试称「兼容旧调用」） — [GRAY?]
- web/src/__tests__/agent-core-isolation.test.ts:67 — 测试注释：事件未带 core（单上游/旧 core）时不过滤 — [GRAY]
- web/src/__tests__/checklist-context-core-scoping.test.ts:140-143 — 测试：旧 core 无 team 标注回退 `(core, session)` 键 — [REMOVE]
- web/src/__tests__/checklist-context-core-scoping.test.ts:91-94 — 测试：旧 Core 缺 `session_id` 归入 `sessionKey(core,'')` — [GRAY?]

## DATA：浏览器 localStorage 历史数据

- web/src/App.vue:53-63 — 旧视图名（caps / logs / tasks）localStorage 迁移 — [DATA]
- web/src/trunk-cache.ts:24-28,107-116 — 缓存内旧格式就地升级（`<subagent_event>` 用户消息 → subagent 行） — [DATA]
- web/src/trunk-cache.ts:95-99 — 旧版/损坏缓存忽略 + `VERSION` 不匹配整体丢弃 — [DATA]
- web/src/__tests__/trunk-cache.test.ts:75-89 — 就格式升级测试（旧缓存子代理钩子） — [DATA]

## 文档/文案性

- web/src/components/SettingsView.vue:850 — 文案「回退内置循环，行为等同旧版内置实现」 — [DOC]
- web/src/components/AgentConfigModal.vue:219 — 注释「保持与旧版一致的初始视图」 — [DOC]
- web/src/components/SettingsView.vue:1116 — 注释「兼容旧引用：capabilityToolOptions 现在等价于全部工具平铺」 — [DOC]
