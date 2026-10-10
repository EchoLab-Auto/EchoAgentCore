# EchoAgentCore 旧版本兼容技术债审计（只读）

范围：`source/**/*.rs`、`config/` 样例、`document/*.md`。分类：[REMOVE]=纯旧版/旧配置/旧对端兼容可移除；[DATA]=用户自身历史数据/存档的旧格式迁移；[GRAY]=需人工判断（存疑加 ?）；[DOC]=文档中的兼容说明。
说明：同一兼容机制跨多行的，主条目给出关键行；测试与被测实现各自成条便于拆分清理。

## source/federation（Core↔Core 线协议）

source/federation/echo-federation/src/frame.rs:8 — 帧协议约定「新增字段一律 serde(default)，新旧节点混部时旧端点可解码新帧」 — [DOC]
source/federation/echo-federation/src/frame.rs:85 — NodeHello.node_name 的 serde(default)+skip（旧 Hello 缺字段容忍；当前 None 也会省略） — [GRAY]?
source/federation/echo-federation/src/frame.rs:89 — NodeHello.version 的 serde(default)：当前发送方恒写该字段，default 仅服务旧端点 — [REMOVE]
source/federation/echo-federation/src/frame.rs:91 — NodeHello.caps 的 serde(default)：当前发送方恒写该字段，default 仅服务旧端点 — [REMOVE]
source/federation/echo-federation/src/frame.rs:98 — NodeHello.advertise 的 serde(default)+skip（当前 None 省略 + 旧对端缺字段容忍） — [GRAY]?
source/federation/echo-federation/src/frame.rs:202-205 — QueryRequest.team_id serde(default)，空值=旧行为（逐 persona 取首个命中，歧义） — [REMOVE]
source/federation/echo-federation/src/frame.rs:348 — 测试注释「旧大脑缺 team_id 的查询可解码」 — [REMOVE]
source/federation/echo-federation/src/frame.rs:385 — 测试注释「旧端点（缺字段的 JSON）可解码」 — [REMOVE]
source/federation/echo-federation/src/frame.rs:387-395 — 测试 invoke_request_decodes_without_new_fields（旧端点缺 args/workdir/timeout 可解码） — [REMOVE]
source/federation/echo-federation/src/frame.rs:396-404 — 测试 query_request_decodes_without_team_id — [REMOVE]
source/federation/echo-federation/src/frame.rs:427-429 — 测试用例「老端点缺 caps/version/node_name/advertise 也可解码」 — [REMOVE]
source/federation/echo-federation/src/frame.rs:221-223 — QueryKind::WorkspaceGitStatus 注释：旧对端不认识该变体时整帧丢弃、降级为不可采集 — [GRAY]?

## source/core（Core 组合根）

source/core/src/config.rs:73 — CoreConfig.server 旧配置段（自动迁移到 [adapters.qq.server]） — [REMOVE]
source/core/src/config.rs:75 — CoreConfig.bot 旧配置段（自动迁移到 [adapters.qq]） — [REMOVE]
source/core/src/config.rs:139 — QqSection 注释：未配置 instances 时按 legacy 单实例 qq 解析 — [GRAY]?
source/core/src/config.rs:313-325 — migrate_plugin_lists 文档：旧编排/驱动 id 归一化 + per-persona 黑名单物化（本函数即旧配置迁移器） — [REMOVE]
source/core/src/config.rs:336 — 迁移日志「migrated legacy loop plugin ids → loop.{single,parallel}」 — [REMOVE]
source/core/src/config.rs:347-352 — 旧 disabled_plugins 黑名单物化进白名单并清空（旧配置兼容） — [REMOVE]
source/core/src/config.rs:405-409 — 注释「内存迁移，首次落盘自愈（与 legacy [server]/[bot] 迁移同范式）」 — [REMOVE]
source/core/src/config.rs:420-440 — Backward compat 分支：[adapters.qq] 未启用时回退旧 [server]/[bot] 段 — [REMOVE]
source/core/src/config.rs:431 — 加载日志「using legacy [server] config — consider migrating」 — [REMOVE]
source/core/src/config.rs:447 — ECHO_ACCESS_TOKEN「Also apply to legacy」写回旧 server 段 — [REMOVE]
source/core/src/config.rs:488-506 — 测试 legacy_federation_enabled_field_is_ignored（旧 enabled=false 必须被忽略） — [REMOVE]
source/core/src/config.rs:574-609 — 测试 migrate_plugin_lists_normalizes_legacy_ids_and_materializes_blacklists — [REMOVE]
source/core/src/config.rs:624-643 — 测试 legacy_config_migrates_to_qq_adapter（旧 [server]/[bot] 迁移） — [REMOVE]
source/core/src/main.rs:192-194 — 注释「联邦始终开启（已取消 enabled 开关）」 — [DOC]
source/core/src/main.rs:609-612 — 兼容旧单实例路径：取第一个实例当默认 qq_adapter — [GRAY]?
source/core/src/main.rs:941-951 — 旧 echo-sessions.json 首次启动自动改名迁移为 echo-sessions-default.json — [DATA]
source/core/src/main.rs:1419-1421 — 「legacy 变量」残留（let _ = first 的 no-op） — [REMOVE]
source/core/src/main.rs:2418-2425 — Query(SessionSnapshot) team_id 为空时退旧行为（逐 persona 取首个命中） — [REMOVE]
source/core/src/qq_instances.rs:13 — 模块文档：单实例（id=qq）沿用 legacy 端口 3131/3000/6099，行为与旧版一致 — [GRAY]?
source/core/src/qq_instances.rs:47-58 — legacy 实例端口缺省直接沿用（OneBot/WebUI 不探测） — [GRAY]?
source/core/src/qq_instances.rs:111 — 注释：legacy 实例容器名/地址属于「既有部署」，实例段未覆盖时沿用 — [GRAY]?
source/core/src/qq_instances.rs:236-256 — legacy 快速路径：无实例表且共享 enabled → 单实例 qq — [GRAY]?
source/core/src/qq_instances.rs:267-272 — 默认人格自动实例沿用 legacy id/容器/端口且不写回配置 — [GRAY]?
source/core/src/qq_instances.rs:327-343 — 测试 legacy_single_instance_keeps_ports_and_id — [GRAY]?
source/core/src/qq_instances.rs:349-360 — 测试 legacy_instance_keeps_shared_container_and_urls — [GRAY]?
source/core/src/qq_instances.rs:441-450 — 测试默认人格在他人格实例已写回时仍沿用 legacy id — [GRAY]?

## source/backend/echo-agent

source/backend/echo-agent/src/config.rs:24 — ThinkingMode 注释「Defaults to enabled for old profiles」（旧 profile 缺字段默认） — [GRAY]?
source/backend/echo-agent/src/config.rs:27 — ReasoningEffort 注释「Defaults to max for old profiles」（旧 profile 缺字段默认） — [GRAY]?
source/backend/echo-agent/src/config.rs:129 — TeamMember 文档标注旧名 `[agent.profiles.{id}]`（未见别名/迁移实现，疑陈旧） — [DOC]
source/backend/echo-agent/src/config.rs:152-159 — TeamMember.disabled_plugins「DEPRECATED，仅保留反序列化供加载期迁移，不再写回」 — [REMOVE]
source/backend/echo-agent/src/config.rs:202-207 — loop_mode 注释含旧编排模式 id 判定（依赖 PARALLEL_MODE_IDS 中的旧 id） — [REMOVE]
source/backend/echo-agent/src/config.rs:300-303 — AgentConfig.memory_limit「DEPRECATED，仅为配置兼容反序列化保留」 — [REMOVE]
source/backend/echo-agent/src/config.rs:457-458 — effective_memory_limit_tokens 文档：legacy memory_limit 不再参与计算 — [REMOVE]
source/backend/echo-agent/src/config.rs:512-529 — 测试 legacy_config_defaults_to_enabled_max_reasoning（旧 profile 缺 thinking/reasoning 字段的默认） — [GRAY]?
source/backend/echo-agent/src/config.rs:642-646 — 测试 legacy_shared_context_is_ignored_and_not_serialized（依赖 serde 忽略未知字段） — [REMOVE]
source/backend/echo-agent/src/config.rs:654-659 — 测试注释「Legacy message-count field no longer participates」 — [REMOVE]
source/backend/echo-agent/src/config.rs:818-823 — 测试「任一旧编排模式 id → Parallel（向后兼容推导）」 — [REMOVE]
source/backend/echo-agent/src/config.rs:845-856 — 测试「已废弃插件黑名单不影响推导（含旧 id）」 — [REMOVE]
source/backend/echo-agent/src/config.rs:860-870 — 测试 team_member_blacklist_is_deserialization_only（旧字段反序列化容忍、不写回） — [REMOVE]
source/backend/echo-agent/src/plugins.rs:23-26 — LEGACY_CHECKLIST_PLUGIN_ID（已移除插件的 id，仅供迁移剔除） — [REMOVE]
source/backend/echo-agent/src/plugins.rs:33-35 — LEGACY_MENU_PLUGIN_ID（同上） — [REMOVE]
source/backend/echo-agent/src/plugins.rs:49-50 — LEGACY_LOOP_RUNNER_PLUGIN_ID（旧驱动 id，迁移剔除） — [REMOVE]
source/backend/echo-agent/src/plugins.rs:52-56 — LEGACY_CHATBOT_MODE_IDS[3]（旧编排模式 id，迁移映射） — [REMOVE]
source/backend/echo-agent/src/plugins.rs:58-63 — PARALLEL_MODE_IDS 把 3 个旧 id 视为 parallel — [REMOVE]
source/backend/echo-agent/src/plugins.rs:65-103 — normalize_mode_plugins：旧 id 折叠/剔除（配置加载与 SaveTeam 防御共用） — [REMOVE]
source/backend/echo-agent/src/plugins.rs:150-178 — convert_plugin_blacklist_to_whitelist：旧黑名单→白名单物化 — [REMOVE]
source/backend/echo-agent/src/plugins.rs:360-389 — 测试 normalize_mode_plugins_maps_legacy_ids — [REMOVE]
source/backend/echo-agent/src/plugins.rs:391-405 — 测试 normalize_mode_plugins_drops_legacy_menu_and_checklist — [REMOVE]
source/backend/echo-agent/src/agent_manager.rs:74-96 — 无 profiles 配置时 legacy 单默认 agent（当前默认兜底行为） — [GRAY]?
source/backend/echo-agent/src/agent_manager.rs:226-228 — save_profile 归一化旧 id「防御旧 panel 回写」 — [REMOVE]
source/backend/echo-agent/src/agent_manager.rs:329-331 — TeamInfo is_default「过渡期保留恒 false，下个协议版本删除」 — [REMOVE]
source/backend/echo-agent/src/agent_manager.rs:334-338 — 注释「旧字段过渡期同时下发」（旧字段已不在线格式中，疑陈旧） — [DOC]
source/backend/echo-agent/src/agent_manager.rs:448 — 测试注释「新字段与旧（过渡）字段必须一致」（旧字段已删除，疑陈旧） — [DOC]
source/backend/echo-agent/src/agent_manager.rs:458-483 — 测试 save_profile_normalizes_legacy_mode_plugin_ids — [REMOVE]
source/backend/echo-agent/src/agent_manager.rs:485-500 — 测试 saved_profile_ignores_deprecated_plugin_blacklist — [REMOVE]
source/backend/echo-agent/src/lib.rs:69-79 — 重组前的兼容 re-export（公开路径保持原样） — [GRAY]?
source/backend/echo-agent/src/packages/mod.rs:25 — 注释：echo_agent::tool::… / subagent::… 经 lib.rs 兼容 re-export — [GRAY]?
source/backend/echo-agent/src/packages/tool/mod.rs:18 — 兼容既有 `tool::builtin` 路径的 re-export 说明 — [GRAY]?
source/backend/echo-agent/src/timeline.rs:137 — 注释「backend 条目不再附加 reasoning（旧数据仍保留字段兼容）」 — [DATA]
source/backend/echo-agent/src/timeline.rs:194-198 — 工具结果配对：旧版对端（tool_call_id 为空）回退按名字匹配 — [GRAY]?
source/backend/echo-agent/src/shell.rs:312-313 — ensure_owned_by 对无归属会话/请求者（legacy 面板会话/旧路径）保守放行 — [GRAY]?
source/backend/echo-agent/src/session.rs:21 — DEFAULT_QQ_INSTANCE「legacy 单实例」常量 — [GRAY]?
source/backend/echo-agent/src/session.rs:55-56 — 注释：多实例 @account 与旧格式天然区分、单实例既有会话 id 不变 — [DATA]
source/backend/echo-agent/src/session.rs:102-110 — SessionKey::parse 兼容旧 `user_{digits}` 会话 id — [DATA]
source/backend/echo-agent/src/session.rs:494-496 — 时间线增量：seq==0 的旧版/重启前条目不参与缺口判断 — [GRAY]?
source/backend/echo-agent/src/session.rs:590-607 — v6 持久化仍写 trunk_histories 投影「为旧读者兼容」（当前无读取方） — [REMOVE]
source/backend/echo-agent/src/session.rs:623-625 — deserialize 文档：v4 及更早迁移进事件日志，保留旧历史兼容 — [DATA]
source/backend/echo-agent/src/session.rs:641-654 — v5 及更早事件无归属→加载期归因迁移 — [DATA]
source/backend/echo-agent/src/session.rs:696-731 — v4 及更早调 migrate_v4_document 兼容读取 — [DATA]
source/backend/echo-agent/src/session.rs:755 — 兜底回退 deserialize_legacy — [DATA]
source/backend/echo-agent/src/session.rs:759-760 — 旧版文件条目没有 seq（反序列化为 0）的容忍 — [DATA]
source/backend/echo-agent/src/session.rs:816-817 — 旧档 id 带 @account 后缀/缺字段按默认实例解析 — [DATA]
source/backend/echo-agent/src/session.rs:828 — 旧档无 team_id 字段时保持 None — [DATA]
source/backend/echo-agent/src/session.rs:848-918 — deserialize_legacy（v1/v2/v3 旧格式加载与迁移） — [DATA]
source/backend/echo-agent/src/session.rs:1592 — session_id_from_hook_content（旧版事件归因迁移用） — [DATA]
source/backend/echo-agent/src/session.rs:1649-1700 — attribute_legacy_events 文档（v5 及更早日志归因规则） — [DATA]
source/backend/echo-agent/src/session.rs:1704-1780 — attribute_legacy_events 实现 — [DATA]
source/backend/echo-agent/src/session.rs:1863-1980 — 旧版事件归因迁移测试（splits/falls_back） — [DATA]
source/backend/echo-agent/src/session.rs:2043-2060 — 测试 session_key_parse_legacy — [DATA]
source/backend/echo-agent/src/session.rs:2374-2395 — 测试 v1/v2 旧格式迁移 — [DATA]
source/backend/echo-agent/src/session.rs:2728-2749 — BASELINE 测试：v4 legacy serializer 的有损往返（注释称仅为 v4 兼容写保留） — [GRAY]?

## source/session/echo-session

source/session/echo-session/src/lib.rs:26 — 模块表：legacy = pre-event-sourced `echo-sessions.json` v4 迁移 — [DATA]
source/session/echo-session/src/lib.rs:31 — `pub mod legacy;` — [DATA]
source/session/echo-session/src/lib.rs:39 — 导出 migrate_v4_document / MigrationError — [DATA]
source/session/echo-session/src/legacy.rs:1-12 — 模块文档：旧 v1–v4 全局 trunk 格式迁移 — [DATA]
source/session/echo-session/src/legacy.rs:31-52 — LegacyDocument 旧格式形状（v1 数组/v2 shared/v3 trunk/v4 timeline） — [DATA]
source/session/echo-session/src/legacy.rs:92-131 — migrate_v4_document 入口 — [DATA]
source/session/echo-session/src/legacy.rs:134-190 — migrate_v1_array / migrate_history（旧消息→事件，工具结构降级） — [DATA]
source/session/echo-session/src/legacy.rs:193-250 — 迁移测试（v1/v2/global trunk/空档） — [DATA]
source/session/echo-session/src/event.rs:12 — 注释：on-disk 格式字段须稳定、新字段需 serde(default) — [GRAY]?
source/session/echo-session/src/event.rs:47 — UserMessage.session「None = 旧版事件（加载期归因迁移）」 — [GRAY]?
source/session/echo-session/src/event.rs:78-107 — Assistant/Tool 事件 session「None = 旧版事件」 — [GRAY]?
source/session/echo-session/src/event.rs:119-121 — CompactionEvent 文档：pre-2026-09 writer 自嵌 marker，投影时只补一次 — [DATA]
source/session/echo-session/src/event.rs:131 — CompactionEvent.session「None = 旧版全局压缩」 — [DATA]
source/session/echo-session/src/event.rs:139-149 — render_compaction_summary 对已含 marker 的旧摘要直接透传 — [DATA]
source/session/echo-session/src/event.rs:387-395 — 测试 legacy summaries 只保留一个 marker — [DATA]
source/session/echo-session/src/event.rs:401-407 — 测试 pre-2026-09 文件缺 archive 字段仍可解析 — [DATA]
source/session/echo-session/src/derive.rs:135 — 注释：session=None 保留未归因事件（旧版日志按此投影） — [DATA]
source/session/echo-session/src/derive.rs:577 — 测试 legacy summaries 单 marker — [DATA]

## source/protocol/echo-protocol

source/protocol/echo-protocol/src/lib.rs:19 — 文档：新增字段 serde(default) 让旧对端可解码 — [DOC]
source/protocol/echo-protocol/src/lib.rs:35 — 导出已退役的 OrchestrationMode（仅旧前端兼容） — [REMOVE]
source/protocol/echo-protocol/src/event.rs:71 — SessionInfo.node_id 注释「单节点/旧 Core 缺省 None」 — [GRAY]?
source/protocol/echo-protocol/src/event.rs:153 — TimelineTool.tool_call_id「empty = legacy entry」 — [DATA]
source/protocol/echo-protocol/src/event.rs:177 — TimelineMessage.session_id「Empty = legacy entries」 — [DATA]
source/protocol/echo-protocol/src/event.rs:191 — TimelineMessage.seq「0 = 旧版/重启前条目」 — [DATA]
source/protocol/echo-protocol/src/event.rs:380-404 — BackgroundTask{Started,Completed,Integrated} 保留事件（Core 侧体系已移除、当前无发射方，因旧消费者仍消费而保留枚举） — [GRAY]?
source/protocol/echo-protocol/src/event.rs:412 — ToolCall.tool_call_id「empty = legacy peer (pre-id protocol)」 — [GRAY]?
source/protocol/echo-protocol/src/event.rs:416 — ToolCall.branch_id「empty for pre-branch legacy paths」 — [GRAY]?
source/protocol/echo-protocol/src/event.rs:430 — ToolResult.tool_call_id「empty = legacy peer」 — [GRAY]?
source/protocol/echo-protocol/src/event.rs:437 — ToolResult.branch_id「empty for pre-branch legacy paths」 — [GRAY]?
source/protocol/echo-protocol/src/event.rs:542 — ContextSnapshot.session_id「None = 旧 Core 的全局口径」 — [GRAY]?
source/protocol/echo-protocol/src/event.rs:559 — ShellSessionsList.team_id「旧 core 无此字段时前端不过滤」 — [GRAY]?
source/protocol/echo-protocol/src/command.rs:42 — SwitchApi 全局激活已被 persona 级 api_profile 取代，协议字段保留兼容、UI 不再暴露 — [GRAY]?
source/protocol/echo-protocol/src/event.rs:873-891 — OrchestrationMode 已退役枚举 + From<LoopMode>（旧前端兼容；当前无消费者） — [REMOVE]
source/protocol/echo-protocol/src/event.rs:908-909 — TeamInfo.is_default 过渡期字段（恒 false，下个协议版本删除） — [REMOVE]
source/protocol/echo-protocol/src/event.rs:923-927 — 注释引用已删除的旧三布尔（reply_branches_enabled 等，2026-09 协议变更） — [DOC]
source/protocol/echo-protocol/src/event.rs:1047-1052 — WorkspaceDirectory untagged：旧格式纯字符串可反序列化，读旧写新 — [DATA]
source/protocol/echo-protocol/src/event.rs:1259-1270 — 测试 directories_accept_legacy_strings_and_qualified_tables — [DATA]
source/protocol/echo-protocol/src/command.rs:321 — UpdateQqAllowlist.adapter「None = 唯一实例/legacy qq」 — [GRAY]?
source/protocol/echo-protocol/src/command.rs:391 — MigrateSession.team_id None = 逐 persona 查找「兼容旧前端」 — [REMOVE]
source/protocol/echo-protocol/src/bridge.rs:252-270 — 测试 legacy_agent_output_without_branch_id_still_decodes（旧 Core 序列化缺 branch_id） — [REMOVE]
source/protocol/echo-protocol/src/bridge.rs:363-377 — 测试 session_info_without_node_id_deserializes（旧 Core SessionInfo 无 node_id） — [REMOVE]

## source/backend/echo-adapter-qq

source/backend/echo-adapter-qq/src/adapter/qq/convert.rs:10-17 — convert_message「Legacy/测试兼容入口」，生产走 convert_message_for — [GRAY]?
source/backend/echo-adapter-qq/src/adapter/qq/convert.rs:21-22 — 注释：convert_message 保留默认实例兼容（测试与 legacy 路径） — [GRAY]?
source/backend/echo-adapter-qq/src/adapter/qq/mod.rs:5 — DEFAULT_INSTANCE_NAME「默认（legacy）QQ 实例名」 — [GRAY]?
source/backend/echo-adapter-qq/src/adapter/qq/mod.rs:57 — persona「None = legacy 单实例未指定」 — [GRAY]?
source/backend/echo-adapter-qq/src/adapter/qq/mod.rs:129 — QqAdapter::new「Legacy 单实例构造」 — [GRAY]?
source/backend/echo-adapter-qq/src/adapter/qq/mod.rs:1399-1401 — 测试：默认实例兼容路径仍为 qq — [GRAY]?
source/backend/echo-adapter-qq/src/file_bridge.rs:128-133 — Backwards-compatible resolve() 单值入口（现仅测试调用） — [GRAY]?

## source/defs / 其他 crate

source/defs/echo-defs/src/mode.rs:8 — GateMode 注释「保持 legacy string-based protocol 的 wire 兼容」 — [DOC]
source/backend/echo-agent/src/packages/workspace/commands.rs:19-21 — 远程 git 状态占位（对端离线/旧版/失败降级） — [GRAY]?
source/backend/echo-agent/src/packages/workspace/commands.rs:225-227 — 注释：对端旧版不认识 WorkspaceGitStatus 时降级为占位条目 — [GRAY]?

## config 样例

config/echo-agent-core.toml:31 — 「旧字段 memory_limit（按消息条数）已废弃，不再参与计算」 — [DOC]
config/echo-agent-core.toml:39 — 「不配置 teams = 单默认 agent，行为与旧版一致」 — [DOC]

## document/*.md

document/architecture.md:55 — echo-session 职责含「v1-v4 兼容迁移」 — [DOC]
document/architecture.md:65 — 旧 crate re-export echo_defs 类型「保持 echo_agent::… 路径兼容」 — [DOC]
document/architecture.md:110 — 「v1-v4 旧格式加载期自动迁移」 — [DOC]
document/core-agent-loop.md:37 — 插件黑名单已移除，推导只看白名单；旧 id 迁移见配置持久化 — [DOC]
document/core-agent-loop.md:127 — CancelRequestedWork team_id「#[serde(default)] 向后兼容」 — [DOC]
document/core-agents.md:25 — 旧 echo-sessions.json 首次启动自动改名迁移 — [DOC]
document/core-agents.md:88 — 含 loop.parallel（或旧编排模式 id）= parallel — [DOC]
document/core-agents.md:108-109 — 旧编排模式 id/旧驱动 id 加载期自动迁移 — [DOC]
document/core-agents.md:117 — 无 [agent.teams] 的旧配置=单 agent；旧协议无 team_id 被拒绝 — [DOC]
document/core-config-persistence.md:82 — legacy [server]/[bot] → [adapters.qq]（显式值才触发） — [DOC]
document/core-config-persistence.md:85-90 — 旧插件 id 归一化 + SaveTeam 防御旧 Panel 回写 — [DOC]
document/core-config-persistence.md:185 — 旧会话文件改名 + v5/更旧格式自动迁移 — [DOC]
document/core-memory.md:28 — v1-v4 旧格式加载期自动迁移 — [DOC]
document/federation.md:106 — 旧配置 enabled=false 会被忽略（字段已移除） — [DOC]
document/federation.md:154 — 不含 advertise 的旧版对端退回单向语义 — [DOC]
document/federation.md:229 — MigrateSession.team_id serde(default) 兼容旧前端 — [DOC]
document/federation.md:282 — 旧 Core 缺省 None，serde 默认兼容 — [DOC]
document/protocol.md:20 — 后端 crate re-export 保持 echo_agent::… 路径兼容 — [DOC]
document/protocol.md:62 — SwitchApi 全局激活已被 persona 级选用取代，「协议字段保留兼容，UI 不再暴露」 — [DOC]
document/protocol.md:93 — ApiBalanceResult 分账字段 serde(default) 对旧前端兼容 — [DOC]
document/protocol.md:99 — BackgroundTask* 保留事件（Core 无发射方、TUI 仍在消费故不删） — [DOC]
document/protocol.md:125 — 旧端帧中 disabled_plugins 被 serde 忽略 — [DOC]
document/protocol.md:143 — OrchestrationMode「旧编排模式兼容名」（TeamInfo 字段已移除） — [DOC]
document/protocol.md:179-183 — 兼容性规则：旧 Core 缺字段默认值、旧端未知变体整帧丢弃 — [DOC]
document/adapter-qq-gating.md:159-165 — legacy 单实例语义与默认人格恒为 qq — [DOC]
document/adapter-qq-gating.md:126 — 枚举 snake_case 序列化「与旧版 wire 格式完全兼容」 — [DOC]
document/panel-qq-tasks.md:17 — 兼容旧 Core：无 persona 元数据时退化展示 — [DOC]
document/panel-qq-tasks.md:19 — 单实例时行为与旧版一致 — [DOC]
document/panel-qq-tasks.md:29 — 后台/并行标签与 awaiting/integrated 状态为「保留适配」（Core 侧体系已移除） — [DOC]
document/panel-chat.md:29 — 历史消息遗留 data URI 仍兼容（Core 加载期迁移） — [DOC]
document/panel-chat.md:82 — 旧 core 无 id 时按名回填最新 running — [DOC]
document/dev-guide.md:124 — EchoAgentTui CI 以 ECHO_CORE_REF 钉住协议兼容 commit — [DOC]
document/dev-testing.md:25-26 — 测试覆盖 legacy payload decoding（缺 branch_id） — [DOC]
document/dev-testing.md:41 — 测试覆盖 qq_instances legacy 端口/共享容器 — [DOC]
document/decoupling-plan.md:75-76 — MCP 换行分帧兼容模式 + FedFrame serde(default) 前向兼容 — [DOC]
document/decoupling-plan.md:228 — compatible() 版本协商 + 不兼容可读拒绝 — [DOC]
document/decoupling-plan.md:315 — setter 侧「旧 setter 兼容期双写」 — [DOC]

## 核验备注（非清单项）

- `echo-federation/src/link.rs:632-644` 的 protocol_version 校验为**严格相等**（当前行为，未见旧版本容忍分支）；`echo-plugin-api/src/protocol.rs:17-19` 的 `compatible()` 在 v1 为严格相等——均为当前协商/拒绝逻辑，不属旧版兼容债。相关「版本不兼容拒绝」测试（conformance/tests）同为当前行为守护。
- 第三方端点类「兼容」措辞（DeepSeek/Anthropic/OpenAI 兼容端点、Kimi、`echo-sanitize` 网关、`echo-llm-*` 实现）与「provider 容错解析」不属本类债务，未列入。
- `config/plugins.example.toml` 无命中；`document` 中「已移除/已取消」多为已完成的能力下线或状态文案，未列入。
