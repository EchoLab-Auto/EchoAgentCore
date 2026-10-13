# 系统审计与修复记录（2026-10）

**时机**：联邦端口合并与 Panel 加载优化收口后，对三仓库做一次全量深审。
**方法**：五路隔离子代理只读深审（Core 运行时 / 基础设施 / 适配器层 /
Panel 前端 / 工程化），主线程逐条核验（file:line 抽查 + serde/行为实测 +
运行时验证），核验属实者**当场修复**（全带回归测试）。
**产出**：本目录记录「已修复」与「待决策」两类清单。行号以 2026-10-13
工作时点为准，可能随代码演进漂移。

## 一、已修复（三仓 23 笔提交，全部 CI 绿）

### Core（EchoAgentCore）

| 提交 | 级别 | 内容 |
| --- | --- | --- |
| `ac71b23` | P1×3 | 联邦出站写 30s 超时（对端不读时冻结 select 循环的僵尸链路）；邀请串 token 改加密级熵源（Uuid::new_v4；配对即全权限）；插件事件缓冲封顶 1024 + 丢弃计数 |
| `5e51439` | P1×2 P2×2 | QQ 连接事件实例名（多实例串台）；GateMode/Owner 多实例广播（共享配置段分叉）；file_bridge 4 处阻塞调用转 spawn_blocking；git 安装 120s 硬超时 + 慢速中止 + 输出重定向防管道自锁 |
| `ad33da4` | — | docker 探测缓存改实例级（修上一笔引入的进程级 static——被 `no_global_state` 棘轮守护拦下，治理有效性实证） |
| `871882b` | P1×3 | `clear_history` 补取 `compact_lock`（压缩窗口内清空被 replace_all 复活）；`shell_emit_for_self` 去 unsafe（改 `emit_handle` 安全出口）；会话导入缓冲三防线（TTL 清扫 / 并发上限 32 / 单块 192KB 校验） |
| `ac2aa1a` | P1 | LLM 流式接收端关闭即中止（不再静默读完整流烧 token；两 provider + 假 SSE 服务器回归测试；protocol.md 同步） |
| `a6eeb64` | P1 | QQ 管理能力下沉为扩展 trait（`QqManagement`+`QqInstanceInfo`；Adapter 面 -14 方法 -2 死方法 +1 访问器；命令层不再静默 no-op；bind_address 顺带修复；能力契约测试 ×2） |
| `ad35015` | — | unit 命令两形态解析契约测试（与 Panel 镜像升级成对） |
| `3c0192b` `796b22f` `4fabd77` `64d2f54` `7af76a1` | — | 治理与文档：仓库地址统一 GitHub、MIT LICENSE、测试计数校准、回滚文档校准、写超时/token 熵源口径同步 |

### Panel（EchoAgentPanel）

| 提交 | 内容 |
| --- | --- |
| `76ff48e` | 联邦 4 命令类型补齐（9 处双重断言清零）+ protocol-sync 三条守护（命令存在性 / 序列化形态契约 / 归零棘轮） |
| `41c2f09` | 智能体编辑补脏检查（快照 + 四接入点确认 + 底栏脏状态；+3 测试） |
| `5b7a65c` | 死状态清理（state.model/systemPrompt）+ PanelSidebar 异步等待修 flaky（全量 3 轮连过） |
| `41f8007` `7221f4a` `0c58848` `e83cca6` | 损坏字符、LICENSE、CI 补 lint:styles、测试计数校准 |

### ui-frame

| 提交 | 内容 |
| --- | --- |
| `e7fe9b7` | CJK 正则两处肉眼不可辨错写（U+FFFD / U+8C48→U+F900；与 echo-defs::token.rs 口径对齐）+ 文档损坏字符清理 |
| `e243d21` | NeumorphismPixelFont 组件收尾提交 |

### 修复统计
- **P1 级真实缺陷 15 项**（写超时僵尸链路、token 弱熵、事件无界、多实例串台/分叉、
  async 阻塞、git 无超时、压缩竞态、unsafe 延寿、导入缓冲三防线、连接事件硬编码、
  LLM 流式白烧 token、协议镜像漂移、适配器抽象泄漏）
- P2 / 治理 / 文档 12+ 项（含 team 编辑脏检查）
- 全部附回归测试或行为验证；`no_global_state` 棘轮实战拦截 agent 违规一次

## 二、待决策清单（已核验属实，未动代码）

### P1
1. **ui-frame Release workflow 15 连败**（2026-09-07 起）：npm 发布停滞 1.3.2-dev.4，
   9 个 changeset 积压。疑似根因：repo 未开启 "Allow GitHub Actions to create and
   approve pull requests" 或 NPM_TOKEN 失效——需仓库设置侧操作后重跑。
2. **QQ hook 失败即丢消息**（echo-adapter-qq/src/handler.rs::deliver）：
   `on_incoming_message` 失败仅 warn；无死信/重试，至少一次语义未定义。

### P2（选摘）
- `router.rs` InvokeOutput 满 64 容量静默丢帧（建议丢帧计数/日志）
- 插件重启预算只增不减（建议时间窗重置）；握手期 HostCall 直接丢弃
- `stdio.rs` shutdown 超时未 killpg（孙进程泄漏；需 libc）
- `napcat/service.rs` 盲删猜测卷名（应从 compose 解析）
- `command.rs::command_clearance` 默认 `_ => Agent`（当前无执行强制点）
- Panel：`ApiSettings` 删除 profile 无二次确认；
  握手队列满 128 静默丢弃；
  ChatView 无虚拟化（库自带 VirtualList 未用）；SettingsView 3654 行待拆
- Core：`main.rs::run_core` 1500 行、`apply_command` 1180 行待拆；
  dev-testing 无 nextest/flaky 治理；CI 无 concurrency/timeout/audit/MSRV

### 架构机会
1. **跨仓协议守护升级**：Core 生成变体清单 → Panel 快照双向比对（本轮
   protocol-sync 已加命令侧守护，全量清单联动仍待做）
2. **巨型单元拆分**：`run_core` / `TrunkStore` / `apply_command` /
   `SettingsView.vue` / `ChatView.vue`（接口边界已在本目录梳理）
3. **背压可观测性一揽子**：写超时（已修）之外，router 丢帧、Panel 事件
   通道 unbounded、federation link 事件通道 64 容量阻塞读循环——统一高水位策略

## 三、正面核验（维持良好）
- `no_global_state` 棘轮、`update_script_plugins`、`config_template`、
  `install_script_flag` 等守护测试体系真实有效（本轮两次实战拦截）
- 事件溯源 + 重投影（event_log 单一事实来源）、replace_all 原子替换、
  幂等重发、脱敏 Aho-Corasick 等设计成熟
- 文档 34 篇零断链；三仓测试基线：cargo 1034 / vitest 369 / ui-frame 1203
