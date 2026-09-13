---
id: qq-gating
title: "QQ 适配器门控"
group: "后端模块 @ 567, 1454, 985, 737"
x: 1283
y: 1636
---
# QQ 适配器门控设计

## 设计目标


在 QQ 消息/以及会话列表相关的信息 到达 agent 之前，通过多层门控机制决定：

1. **哪些消息**进入 agent 处理
2. **哪些群/用户**对 agent 可见
3. 门控规则可**运行时动态调整**，无需重启

* 注意：tools的调用也包含在内，都需要通过这个门控进行过滤

## 门控管道

消息流经五层门控，层层收窄：

```text
原始消息 → 连接认证 → 触发条件 → 过滤管道 → 会话隔离 → agent
                              ↓
                         群/用户列表门控（并行，影响可见性）
```

### 第一层：连接认证

- NapCat 以反向 WebSocket 客户端身份连接
- 可选的 access_token 验证
- 连接状态实时同步至 TUI

### 第二层：触发条件

- **群聊**：仅处理 @机器人的消息（可配置关闭，处理所有群消息）
- **私聊**：可配置自动回复开关
- 不满足条件的消息**静默丢弃**，零开销

### 第三层：过滤管道（责任链模式）

按固定顺序执行的过滤器链，首次不通过即短路：

| 顺序 | 过滤器 | 职责 | 短路策略 |
|---|---|---|---|
| 1 | 管理员绕过 | 群主消息直接放行 | 通过并跳过后续 |
| 2 | 白名单（门控模式 allowlist） | 仅允许列表中的用户/群 | 阻止 |
| 3 | 黑名单（门控模式 denylist） | 禁止列表中的用户/群 | 阻止 |
| 4 | 频率限制 | 每用户/每群/全局速率控制 | 阻止 |
| 5 | 关键词 | 内容匹配阻止 | 阻止（可附带回复） |
| 6 | 长度限制 | 超长消息阻止 | 阻止 |

**设计要点**：
- 白名单和黑名单根据 `GateMode` **互斥生效**——门控模式为 `allowlist` 时管道只含白名单过滤器，`denylist` 时只含黑名单，`none` 时两者都不含
- 白名单为空 = 未启用，所有消息通过
- 管理员完全绕过后续过滤
- 每个过滤器独立判断，互不耦合

### 第四层：来源身份

通过的消息按 `平台:范围:范围ID:用户ID` 创建独立的来源身份：

- 群聊：`qq:group:群号:QQ号`
- 私聊：`qq:dm::QQ号`

这些身份用于区分消息来源、投递目标和授权范围；它们共享 Agent 的全局 Trunk
上下文，不构成记忆隔离边界。

### 第五层：群/用户列表门控

与过滤管道并行运作，控制 agent **可见的**群和用户范围：

- 白名单启用时，`get_group_list()` 仅返回白名单中的群
- 黑名单启用时，`get_group_list()` 排除黑名单中的群
- `get_friend_list` tool 使用相同的用户门控：非空白名单仅返回已放行用户，黑名单模式排除已拒绝用户
- `owner_qq` 保留管理员绕过权限；用户白名单为空时，与消息门控一致，不限制私聊好友
- TUI 管理界面使用有权限的全量列表；LLM tool 不能通过该接口绕过 gate

## 多实例（2026-09-13）

QQ 适配器支持**多实例**：一个实例 = 一个 NapCat 容器 + 一条反向 WS 通道 + 一个账号，
互不冲突；实例归属某个人格（**一个人格可挂多个实例**）。

| 概念 | 说明 |
| --- | --- |
| 实例 id | `instance_id`，全局唯一，即**适配器名**与会话 `@account` 维度 |
| 归属 | `[adapters.qq.instances.<id>].persona = "<team>"` |
| 端口 | 每实例 3 个宿主端口：反向 WS / OneBot HTTP / WebUI（`[...ports]`），**自动分配并持久化** |
| 容器 | `echo-napcat-<id>`，独立数据卷 `echo-napcat-<id>-{data,config}`，compose 由 Core 生成（`<数据目录>/napcat/<id>/docker-compose.yml`） |
| 自动创建 | 人格启用 `echo-agent.adapter.qq` 且无归属实例时自动建档（id = 人格 id，重复则 `<id>-2`…） |

```toml
[adapters.qq]              # 共享默认（镜像/auto_start/路径模板…）
enabled = true

[adapters.qq.instances.alix-two]
persona = "alix"
# 端口首次自动分配后写回（可手工固定）
[adapters.qq.instances.alix-two.ports]
reverse_ws = 3140
onebot_http = 3010
webui = 6100
```

**legacy 单实例（未配置任何 `[...instances]` 段）**：实例 id 为 `qq`、归属首个启用
QQ 的人格；端口沿用 **3131/3000/6099**（OneBot/WebUI 是既有容器的宿主映射，不做探测），
容器与 NapCat 地址沿用共享默认（容器 `napcat`、`localhost:3000/6099`），**不写回配置**
——与多实例之前完全一致（零迁移）。

**路由与隔离**：

- 入站：每实例把消息投给**归属人格**（`AgentMessageHook` 按实例接线），事件带实例名
- 会话键：`qq:group:<gid>:<uid>@<实例>`；实例名为默认 `qq` 时不加后缀（单实例部署零迁移）
- 出站工具：每人格注册**自己实例集合**的 `send_*`/`get_*`；多实例时 schema 增加可选 `account`（实例名），缺省在多实例下报错列出可选值
- 管理面（门控/名单/owner/登录）：命令与事件均带 `adapter` 字段；缺省时若只有唯一实例则回退（旧 Panel 兼容）
- **登录由 Core 代理**：`RequestQqLoginStatus` / `RequestQqQrcode`（二维码以 PNG base64 回推 `QqQrcode` 事件），Panel 不再直连 OneBot HTTP / docker

## 运行时可变性

门控规则分为两类：

| 类别 | 变更方式 | 生效时机 |
|---|---|---|
| 连接/触发条件 | 修改配置文件 + 重启 | 下次启动 |
| 白名单/黑名单 | TUI 交互式命令 | 立即生效 |
| 门控模式 | `/qq setting` 单选切换 | 立即生效 + 持久化 |
| 管理员 owner | `SetQqOwner` 协议命令 | 立即生效 + 持久化 |

白名单/黑名单的运行时修改路径：

```text
TUI 表单选择 → BackendCommand (携带 GateMode 枚举)
  → Agent 分发 → 适配器更新 → rebuild_filter_pipeline() → persist_filter()
    (通过共享的 ConfigStore 原子写入)
```

### 管理员（owner_qq）运行时设置

owner 与门控/名单一样有运行时更新路径，无需重启：

- 经 `BackendCommand::SetQqOwner { owner_qq }` 设置（`0` = 清除）；`QqInner` 持运行时值（初始化自配置），`set_owner_qq` 更新运行时值并经共享 ConfigStore 原子写回 `[adapters.qq] owner_qq`，`get_owner_qq` 读运行时值
- 门控豁免一律读运行时值：过滤管道的「管理员绕过」、`get_gated_friend_list`、出站门控
- `Adapter` trait 提供 `set_owner_qq`/`get_owner_qq` 默认 no-op 方法，QQ 实现覆盖
- **语义边界**：运行时设置的 owner 仅影响门控豁免；自更新授权仍由 `[agent.self_update]` 控制（与 `allowed_qq_users` 的联动未接入——见 [部署与自更新](./ops-deploy.md)）
- `install.sh --owner-qq` 已移除：设置入口为协议命令或直接编辑 `[adapters.qq]`（当前 Web Panel 仅只读显示 owner，未接设置 UI）

### 门控模式类型

`GateMode` 是定义在 `echo-adapter` 中的强类型枚举，跨层使用：

| 枚举值 | TOML 序列化 | 行为 |
|---|---|---|
| `GateMode::None` | `"none"` | 不启用任何过滤 |
| `GateMode::Allowlist` | `"allowlist"` | 仅启用 allowlist 过滤器 |
| `GateMode::Denylist` | `"denylist"` | 仅启用 denylist 过滤器 |

枚举值在 WebSocket JSON 层自动序列化为 snake_case 字符串，与旧版 wire 格式完全兼容。
管道重建时，`build_filter_pipeline_gated(mode)` 根据门控模式**互斥**地只加入 allowlist 或  denylist（不会同时启用两个）。

## 关键设计决策

1. **白名单空 = 全放行**：避免初始配置时意外阻止所有消息，符合最小惊讶原则
2. **黑名单优先于白名单**：允许"大部分允许，少数禁止"的常见场景
3. **管理员全局绕过**：确保机器人 owner 始终可控，不会把自己锁在外面
4. **群列表门控与消息门控共用规则**：保持 agent 看到的世界与它能交互的世界一致
5. **管道可重建不阻塞消息处理**：Mutex 锁仅持有一瞬间（克隆 Arc），不影响消息吞吐
6. **强类型门控枚举**：`GateMode` 枚举在 echo-adapter 定义，全栈类型安全，WebSocket JSON 层用 snake_case 保持兼容
7. **统一配置持久化**：门控配置和 agent 配置共享同一个 `ConfigStore` 实例（main.rs 创建一次分发），并发安全由内部 Mutex 保证
8. **门控模式互斥**：`build_filter_pipeline_gated()` 只加入 allowlist 或 denylist 之一——两者永远不会同时存在于运行中的过滤管道
