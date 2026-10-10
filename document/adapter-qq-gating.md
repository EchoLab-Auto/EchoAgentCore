---
id: qq-gating
title: "QQ 适配器门控"
group: 插件
x: 960
y: 2220
---

# QQ 适配器门控设计

> **定位**：本文是 `echo-agent.adapter.qq` 插件的机制文档——QQ 消息与会话列表信息在到达 agent 之前经过的五层门控、运行时可变性、多实例与容器生命周期、文件接收与语音/文件发送、消息渲染。读者：QQ 适配器的使用者与维护者。插件的装载与 persona 门控见 [插件化设计](./core-plugins.md)；多实例涉及的会话/人格模型见 [多 Agent 与会话](./core-agents.md)。

## 设计目标

在 QQ 消息以及会话列表相关信息到达 agent 之前，通过多层门控机制决定：

1. **哪些消息**进入 agent 处理
2. **哪些群/用户**对 agent 可见
3. 门控规则可**运行时动态调整**，无需重启

- 注意：工具的调用也包含在内，都需要通过这个门控进行过滤

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
- 连接状态实时同步至 Panel

### 第二层：触发条件

- **群聊**：仅处理 @机器人的消息；`group_at_reply = false` 时**群聊整体不响应**（全部群消息丢弃，并非「关闭 @ 检查、处理所有群消息」——handler.rs 的 `!group_at_reply || !at_me` 判定）
- **私聊**：可配置自动回复开关
- 不满足条件的消息**静默丢弃**，零开销

### 第三层：过滤管道（责任链模式）

按固定顺序执行的过滤器链，首次不通过即短路：

| 顺序 | 过滤器 | 职责 | 短路策略 |
|---|---|---|---|
| 1 | 管理员绕过 | 机器人 owner（`owner_qq`）消息直接放行 | 通过并跳过后续 |
| 2 | 白名单（门控模式 allowlist） | 仅允许列表中的用户/群 | 阻止 |
| 3 | 黑名单（门控模式 denylist） | 禁止列表中的用户/群 | 阻止 |
| 4 | 频率限制 | 每用户/每群/全局速率控制 | 阻止 |
| 5 | 关键词 | 内容匹配阻止 | 阻止（`BlockWithMessage` 附带回复为预留能力，当前无过滤器使用） |
| 6 | 长度限制 | 超长消息阻止 | 阻止 |

**设计要点**：
- 白名单和黑名单根据 `GateMode` **互斥生效**——门控模式为 `allowlist` 时管道只含白名单过滤器，`denylist` 时只含黑名单，`none` 时两者都不含
- 白名单为空 = 未启用，所有消息通过
- **群成员放行开关**（`[adapters.qq.filter.allowlist] group_members_open`，缺省 false）：开启后群消息**只校验群名单**——白名单群内全体成员可交互，`user_ids` 此时仅约束私聊；关闭时群消息需同时命中 `user_ids` 与 `group_ids`（“群里只有名单用户能触发”）。启动期开关（改文件 + 重启生效）；运行时编辑名单（Panel）不会丢失该开关（持久化为整表替换时原样带回）
- 管理员完全绕过后续过滤
- 每个过滤器独立判断，互不耦合

### 第四层：来源身份

通过的消息按 `平台:范围:范围ID:用户ID` 创建独立的来源身份：

- 群聊：`qq:group:群号:QQ号`
- 私聊：`qq:dm::QQ号`

这些身份用于区分消息来源、投递目标和授权范围；消息按会话归属写入同一份 append-only
事件日志，模型上下文**按来源会话独立投影**（各来源上下文相互隔离）。

### 第五层：群/用户列表门控

与过滤管道并行运作，控制 agent **可见的**群和用户范围：

- 白名单启用时，`get_group_list()` 仅返回白名单中的群
- 黑名单启用时，`get_group_list()` 排除黑名单中的群
- `get_friend_list` tool 使用相同的用户门控：非空白名单仅返回已放行用户，黑名单模式排除已拒绝用户
- `owner_qq` 保留管理员绕过权限；用户白名单为空时，与消息门控一致，不限制私聊好友
- 群历史查询工具（`get_group_msg_history`）受同一群门控约束：白名单模式下仅可读白名单群
- Panel 管理界面使用有权限的全量列表；LLM tool 不能通过该接口绕过 gate

## 运行时可变性

门控规则分为两类：

| 类别 | 变更方式 | 生效时机 |
|---|---|---|
| 连接/触发条件 | 修改配置文件 + 重启 | 下次启动 |
| 群成员放行开关（`group_members_open`） | 修改配置文件 + 重启 | 下次启动 |
| 白名单/黑名单 | Panel 交互式命令 | 立即生效 |
| 门控模式 | `/qq setting` 单选切换 | 立即生效 + 持久化 |
| 管理员 owner | `SetQqOwner` 协议命令 | 立即生效 + 持久化 |

白名单/黑名单的运行时修改路径：

```text
Panel 表单选择 → BackendCommand (携带 GateMode 枚举)
  → Agent 分发 → 适配器更新 → rebuild_filter_pipeline() → persist_filter()
    (通过共享的 ConfigStore 原子写入)
```

### 管理员（owner_qq）运行时设置

owner 与门控/名单一样有运行时更新路径，无需重启：

- 经 `BackendCommand::SetQqOwner { owner_qq }` 设置（`0` = 清除）；`QqInner` 持运行时值（初始化自配置），`set_owner_qq` 更新运行时值并经共享 ConfigStore 原子写回 `[adapters.qq] owner_qq`，`get_owner_qq` 读运行时值
- 门控豁免一律读运行时值：过滤管道的「管理员绕过」、`get_gated_friend_list`、出站门控
- `Adapter` trait 提供 `set_owner_qq`/`get_owner_qq` 默认 no-op 方法，QQ 实现覆盖
- **语义边界**：运行时设置的 owner 仅影响门控豁免，不联动其它授权（原自更新授权联动已随 `framework_update` 工具废弃移除）
- `install.sh --owner-qq` 已移除：设置入口为协议命令或直接编辑 `[adapters.qq]`（当前 Web Panel 仅只读显示 owner，未接设置 UI）

### 门控模式类型

`GateMode` 定义在 `echo-defs`（经 `echo-adapter` / `echo-protocol` 再导出），跨层使用：

| 枚举值 | TOML 序列化 | 行为 |
|---|---|---|
| `GateMode::None` | `"none"` | 不启用任何过滤 |
| `GateMode::Allowlist` | `"allowlist"` | 仅启用 allowlist 过滤器 |
| `GateMode::Denylist` | `"denylist"` | 仅启用 denylist 过滤器 |

枚举值在 WebSocket JSON 层自动序列化为 snake_case 字符串（wire 格式固定不变）。
管道重建时，`build_filter_pipeline_gated(mode)` 根据门控模式**互斥**地只加入 allowlist 或 denylist（不会同时启用两个）。

## 多实例与容器生命周期

QQ 适配器支持**多实例**：一个实例 = 一个 NapCat 容器 + 一条反向 WS 通道 + 一个账号，
互不冲突；实例归属某个人格（**一个人格可挂多个实例**）。

### 多实例模型

| 概念 | 说明 |
| --- | --- |
| 实例 id | `instance_id`，全局唯一，即**适配器名**与会话 `@account` 维度 |
| 归属 | `[adapters.qq.instances.<id>].persona = "<team>"` |
| 端口 | 每实例 3 个宿主端口：反向 WS / OneBot HTTP / WebUI（`[...ports]`），**自动分配并持久化** |
| 容器 | `echo-napcat-<id>`，独立数据卷 `echo-napcat-<id>-{data,config}`，compose 由 Core 生成（`<数据目录>/napcat/<id>/docker-compose.yml`） |
| 自动创建 | 人格启用 `echo-agent.adapter.qq` 且无归属实例时自动建档（id = 人格 id，重复则 `<id>-2`…）；**默认人格（首个启用者）固定用 legacy id `qq`**，沿用共享容器/端口语义 |
| 显示名 | 实例化：`QQ（<persona> / <实例>）`；实例名为 `qq` 时 `QQ（<persona>）`；未归属时 `QQ / OneBot`——Panel 多实例下据此区分 |
| 持久化 | 自动建档时把 **persona + ports** 一并写回 `[adapters.qq.instances.<id>]`（仅缺字段才写；legacy 实例不写回）。只写端口会让重启后的 persona 缺失、被重新分配给默认人格（归属漂移） |

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

**默认人格的实例恒为 `qq`**：即使其他人格的实例已被写回配置表（legacy 快速路径不再
命中），默认人格也会经自动建档拿到同一份 legacy 语义（id `qq`、共享容器/端口、不写回）
——保证从单实例演进到多 persona 时，首个实例的容器与端口不漂移。

### 路由与隔离

- 入站：每实例把消息投给**归属人格**（`AgentMessageHook` 按实例接线），事件带实例名
- 会话键：`qq:group:<gid>:<uid>@<实例>`；实例名为默认 `qq` 时不加后缀（单实例部署零迁移）
- 出站工具：每人格注册**自己实例集合**的 `send_*`/`get_*`；多实例时 schema 增加可选 `account`（实例名），缺省在多实例下报错列出可选值
- 管理面（门控/名单/owner/登录）：命令与事件均带 `adapter` 字段；缺省时若只有唯一实例则回退（单实例部署）
- **登录由 Core 代理**：`RequestQqLoginStatus` / `RequestQqQrcode`（二维码以 PNG base64 回推 `QqQrcode` 事件），Panel 不直连 OneBot HTTP / docker
- **二维码陈旧自动刷新**：`login_qrcode_png` 先探测容器内 PNG 的 mtime，缺失或 >90s（`QR_MAX_AGE_SECS`）时经 WebUI（`webui.json` token → `sha256(token+".napcat")` → `/api/auth/login` → `/api/QQLogin/RefreshQRcode`）让 NapCat 重新生成、等 2s 落盘再取；刷新失败仅告警并退回读现有文件。NapCat 自身轮换循环停摆时（实测会发生），这是"刷新没反应"的解法；新鲜码（<90s）直接返回，不会作废用户刚扫的码

### 容器生命周期与登录保持

- **napcat_auto_start**（默认 true）：适配器启动时确保 NapCat 容器在运行
  （compose up）；**napcat_auto_stop 默认 false**——适配器停止（含 Core 停机/
  重启）**不**联动停止容器：容器保持运行则 QQ 始终保持在线（NapCat 自身维持
  与 QQ 服务器的连接），Core 重启只断反向 WS、重启后自动重连，用户无感知。
  联动停止会引入两类事故：① 容器重启 = QQ 下线，需重新登录；② 频繁容器重启
  触发 QQ 安全策略使快速登录凭证失效（"用户身份已失效/登录态已失效"）。
  需要联动停止时显式配置 `napcat_auto_stop = true`。
- **启动时自动登录**：适配器监测到 NapCat online 时经 WebUI API 固化登录态
  （`ensure_quick_login`）——已登录则把当前账号写入容器 `webui.json` 的
  `autoLoginAccount`（`docker exec sed` 原地写入），此后容器重启 NapCat 直接
  快速登录；未登录则尝试 `SetQuickLogin`（失败静默回退扫码流程）。
- **快速登录失效的兜底**：NapCat 快速登录凭证被 QQ 安全策略判失效时由扫码流程
  兜底（`ensure_quick_login` 检测未登录即调 `SetQuickLogin`，失败静默回退扫码）——
  此时到 Panel「适配器」面板重新扫码。登录态仅经 `QqLoginStatus`（adapter/online/
  user_id/nickname）同步；二维码失败原因经 `QqQrcode.error` 回传（如「无法刷新
  NapCat 二维码」）。二维码约 2 分钟有效（Core 侧 90s 新鲜度阈值，超龄先让 NapCat
  刷新再取），扫旧码会得到 `ErrCode: 3`（授权超时）。

## 消息收发、历史查询与渲染

QQ 用户发来的文件会**自动下载到本机**，以本地路径随消息送达 agent（hook payload
`files[]`，agent 用文件/命令工具直接读）。

### 文件接收

**来源渠道与去重**（NapCat 对同一份文件可能双上报，按通道去重）：

| 来源 | 事件 | 处理 |
| --- | --- | --- |
| 群文件上传 | `group_upload` 通知 | 经 `get_group_file_url` 换直链后下载；群聊消息里的 `file` 段**跳过**（同一次上传的双上报）。通知无 @ 信息，不受「群聊仅 @」触发条件限制，但照常走过滤管线（白/黑名单等） |
| 私聊文件 | 消息 `file` 段（`file_id`/`url`） | 段带 `url` 直接下载；否则经 `get_private_file_url` 换链下载 |
| 私聊「在线文件/文件夹」（QQ 直传） | 消息 `onlinefile` 段 | **不支持自动接收**（无直链，接收动作后字节仍在 NapCat 侧）：以显式失败条目送达，agent 如实告知用户 |

**配置**（`[adapters.qq.files]`）：

| 键 | 默认 | 说明 |
| --- | --- | --- |
| `dir` | 空 = `~/.local/share/echo-agent-core/downloads` | 保存目录 |
| `max_mb` | 100 | 单文件大小上限；超限跳过下载（条目仍送达并注明原因）；`0` 回退默认 100 |
| `accept_group_upload` | true | 关闭后群文件通知整体丢弃（事件不进 agent） |
| `accept_private_file` | true | 关闭后私聊文件段被剥掉：纯文件消息整体丢弃，文本/图片照常送达 |

**落盘与送达**：

- 文件名 = `<毫秒时间戳>-<消毒后的名字>`：剥路径成分/控制字符/前导点（防穿越、
  防隐藏文件覆盖），`create_new` 独占创建，同毫秒撞名追加序号兜底
- 流式下载：`Content-Length` 预检 + 写入中复检；超限/中断删除半成品；URL 仅接受
  `http(s)`
- 送达：hook payload `files[]`（`name`/`path`/`size`/`error`）+ content 拼一行
  人类可读描述（面板展示）；下载失败/超限**不丢事件**（`path = null` + `error`
  说明原因，agent/用户都能看到）
- 下载发生在过滤管线**通过之后**（被拦截的消息不白拉大文件）

**已知限制**：

- 「在线文件/文件夹」（QQ 直传。NapCat elementType 23/30）与「闪传」不支持接收；
  前者以显式失败条目送达，后者段类型未识别、整条消息丢弃
- 下载在适配器事件分发的串行链路上执行：大文件下载期间该 QQ 实例的后继事件排队
  等待（图片内嵌下载同理，但文件体积上限更大）
- 下载文件不做自动清理，长期运行需自行管理磁盘（目录见配置 `dir`）

### 语音发送（出站）

出站侧有两个易混淆的通道，语义不同（工具描述中同样写明分工）：

| 工具 | 形态 | 用途 |
| --- | --- | --- |
| `send_file` | QQ 文件消息（可下载、可转发） | 用户要「保存/下载文件」 |
| `send_voice` | **QQ 语音气泡**（点开即播，非文件） | 用户要「听」一段音频 |

`send_voice` 的 `file` 参数按以下顺序解析（候选逐个尝试，NapCat 接受即发送成功）：

1. `http(s)://` / `base64://` / `file://` 引用——原样交给 NapCat（自行下载/解码）；
2. 容器内可见的绝对路径（如 `/app/napcat/data/x.mp3`）——直通；
3. **裸文件名**——先查 NapCat 数据目录（`/app/napcat/data/`：此前收发的文件都在
   这里，文件桥 `docker cp` 的落点），命中即用；否则回退按本机路径桥接；
4. 其它本机路径——经文件桥送进 NapCat（`docker cp`，失败退 HTTP 下载 URL）。

常见音频格式（mp3/wav/amr/silk…）由 NapCat 自动转码为 QQ 语音格式；发送沿用与
文本相同的出站门控（`check_outbound_gate`）。注意 QQ 侧语音有**时长/体积限制**，
超限长音频建议改用 `send_file`（写文件消息）。

### 群历史查询（`get_group_msg_history`）

出站侧唯一的「读」工具：拉取群近期消息的**可读转写**，供 agent「补上下文」——
与入站管道不同，它**包含未 @ 机器人的消息、以及被门控过滤掉的消息**（这正是它的用途：
群里有人问「前面聊了什么」但那条没带 @ 时，agent 仍能翻阅）。

- 参数：`group_id`（必填）、`count`（缺省 50，封顶 500）、`since_minutes`（只看最近 N 分钟）
- 渲染（`echo-core::segment::render_transcript`）：每条一行
  `[MM-DD HH:MM:SS] 昵称(QQ号): 正文`；媒体与提及用紧凑标记（`[图片]`/`[语音]`/
  `[文件:名]`/`@昵称`/`[卡片]`/`[转发消息]`…），段解析失败回退 `raw_message`
- **受群可见性门控**（`visible_group_check`，语义与消息门控一致）：白名单模式只允许
  白名单群（空白名单 = 不限制）、黑名单模式排除名单内——「agent 读得到的世界 =
  它能交互的世界」
- 窗口受 QQ 客户端本地消息库限制（长期未活跃的群可能只有很少历史）

### 消息内容渲染

QQ 消息转成 agent 输入时，`content` 走「可读渲染」（`MessageEvent::readable_text`，
echo-core）：

| 段 | 渲染结果 |
| --- | --- |
| `text` | 原样 |
| `face`（QQ 内置表情） | `[表情:微笑]`；未知 id 回退 `[表情:123]`（对照表 `echo-core/src/face.rs`，取自 NapCat `face_config.json`，329 条） |
| `dice` / `rps` | `[骰子:4]` / `[石头剪刀布:剪刀]`（OneBot v11：1 石头、2 剪刀、3 布） |
| `poke` | `[戳一戳]` |
| `image` | 不进 content，经 `images` 通道——**触发门控与过滤管道通过后**下载、**落盘媒体库**、传递 `/media/<id>` 引用（被丢弃的消息不触发下载；见 [Core 框架](./core.md)§多模态输入） |
| `file` / `onlinefile` | 不进 content，经 `files` 通道（见 §文件接收） |
| 其它（`record`/`video`/`xml`/`json`/`forward`…） | 不渲染 |

- **与 `plain_text()` 的分工**：命令解析（`/help` 等）继续读 `plain_text`
  （纯文本，标记不能干扰前缀判断）；只有交给 agent 的 content 用可读渲染。
- 表情此前被整体丢弃：带表情的消息「读不到」表情，**纯表情消息因 content
  为空被整条丢弃**（agent 完全不知道用户发过消息）；渲染后这类消息正常送达。
- 已知限制：纯语音/视频/富卡片（xml/json）消息仍会被整条丢弃（content 为空）；
  如需「至少让对方的话被看到」，可再补 `[语音]`/`[视频]` 占位标记。

## 关键设计决策

1. **白名单空 = 全放行**：避免初始配置时意外阻止所有消息，符合最小惊讶原则
2. **allowlist / denylist 互斥**：由门控模式二选一（`build_filter_pipeline_gated` 只加入 allowlist 或 denylist 之一，两者永远不会同时存在于运行中的过滤管道）——「大部分允许、少数禁止」用 denylist 模式，「严格白名单」用 allowlist 模式
3. **管理员全局绕过**：确保机器人 owner 始终可控，不会把自己锁在外面
4. **群列表门控与消息门控共用规则**：保持 agent 看到的世界与它能交互的世界一致
5. **管道可重建不阻塞消息处理**：Mutex 锁仅持有一瞬间（克隆 Arc），不影响消息吞吐
6. **强类型门控枚举**：`GateMode` 在 `echo-defs` 定义（`echo-adapter` / `echo-protocol` 再导出），全栈类型安全，WebSocket JSON 层用 snake_case 保持兼容
7. **统一配置持久化**：门控配置和 agent 配置共享同一个 `ConfigStore` 实例（main.rs 创建一次分发），并发安全由内部 Mutex 保证
