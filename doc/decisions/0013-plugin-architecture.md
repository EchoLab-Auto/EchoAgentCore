# 0013 Plugin Architecture — 插件化核心架构设计

日期: 2026-08-24
状态: 已实现（Phase 1 骨架）

## 背景与目标

EchoAgentCore 当前是"组合根手工装配"架构：`source/core/src/main.rs` 按顺序
创建 skills / tools / adapters / provider / loop 并组装 Agent。新增能力需要
改代码重新编译。本决策引入**插件化（pluginization）**：所有模块作为可挂载、
可启停、可热重载的插件围绕 Agent 构建，并让 **Agent 自身具备框架自更新能力**
（本决策同时强化 `framework_update` 工具与 update.sh 流程）。

设计原则（沿袭 dsh）：

1. 一切皆插件：LLM provider、TurnRunner(loop)、工具、技能、适配器、编排、
   management server 均为插件，无特权核心模块。
2. 插件 = manifest + 生命周期钩子；注册是可逆副作用（disposer）。
3. 热重载优先于重启：**数据类插件**（技能/工具/清单）秒级热重载；
   **代码类插件**（provider/loop/适配器）通过原子二进制替换 + 进程重启生效
   （Rust 无稳定 ABI，动态库方案被否决，见下）。
4. 自更新是 Agent 的能力：`framework_update` 工具 + systemd oneshot 服务
   （构建→替换→重启），Agent 可查询状态与触发，授权由 `[agent.self_update]`
   控制。

## 为什么不用动态库（.so）插件

Rust 的 ABI 不稳定；`libloading` + C ABI 需要每个插件手写 extern "C" 桥，
长期维护成本高、崩溃诊断难，与"试验性团队快速迭代"目标冲突。
采用**源码级插件 + 进程级热替换**：插件以 crate/目录形式存在于受管仓库或
`plugins/` 目录，更新 = 重新构建二进制 + restart（已有原子替换机制）。
热重载范畴限定为数据类插件（见上）。

## 架构

### 新 crate：`echo-plugin`（Service Definition 层）

```
source/plugin/echo-plugin/
  src/lib.rs        # 重新导出
  src/manifest.rs   # PluginManifest（TOML：id/name/version/kind/entry/options）
  src/registry.rs   # PluginRegistry（发现/加载/启停/列表；复用 echo-context）
  src/plugin.rs     # Plugin trait（kind 判别 + 生命周期钩子）
```

### Plugin trait

```rust
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> &PluginManifest;
    /// 挂载副作用（注册工具/技能/服务）；失败返回错误，注册表跳过该插件。
    fn mount(&self, ctx: &MountContext) -> Result<Vec<Disposer>, PluginError>;
    /// 卸载前清理（默认空实现：靠 disposer 已逆注册）。
    fn unmount(&self) -> Result<(), PluginError> { Ok(()) }
}
```

`MountContext` 提供 agent 可注入的锚点：`ToolRegistry`、`SkillRegistry`、
`Ctx`（服务定位）、`AdapterRegistry`。插件**只依赖定义层**（echo-defs /
echo-plugin），不 import echo-agent（依赖方向单一）。

### 插件种类（kind）

| kind | 载体 | 热重载 | 示例 |
|---|---|---|---|
| `skill` | SKILL.md 目录 | 秒级（已有） | calculator、web-search |
| `tool` | Tool trait 实现 | 秒级（register_reversible） | checklist、send_qr |
| `provider` | LlmProvider | 重启 | deepseek/openai/anthropic/ollama |
| `loop` | TurnRunner | 重启 | 默认 turn runner |
| `adapter` | ChatAdapter | 重启 | qq |
| `orchestration` | 内置编排（任务/分支/定时） | 重启 | background/parallel/subagent |
| `management` | management WS | 重启 | panel bridge |

### 内置插件注册（composition root）

`main.rs` 从"顺序手工代码"变为"注册表驱动"：每个内置模块包装为一个
`BuiltinPlugin{manifest, mount}`。示例：

```rust
registry.register(Arc::new(BuiltinPlugin::new(
    manifest!("echo-agent.tools.builtin", "0.1.0", "builtin", "内置工具集"),
    move |ctx| { register_all_builtin_tools(&ctx.tools, ...); Ok(disposers) },
)));
```

### 清单发现（外置数据插件）

`plugins/{kind}/{id}/plugin.toml` + `SKILL.md`：
- 发现：`PluginRegistry::discover(dir)` 遍历 `plugin.toml`
- 热重载：Agent 已有 1s 技能目录轮询；扩展为对 `plugins_dir` 的通用轮询
  （内容 diff → 重新挂载；旧 disposer 释放 → 逆注册）
- 启用状态持久化：`[agent].disabled_plugins`（与 disabled_skills 同模式）

## 自更新流程（强化）

现有：`framework_update` 工具(status/apply) + `echo-agent-core-update.service`
(oneshot, update.sh)。强化点：

1. **插件感知**：update.sh 构建完成后校验新二进制内含的 builtin 插件清单
   （`echo-agent-core --list-plugins` 或 manifest 嵌入），版本不一致则警告
2. **framework_update status** 输出：build revision、插件清单摘要
   （id/version/kind/enabled）
3. 新增 action=plugins：列出/启停插件（复用 Frontend 命令通道，agent 只读
   状态，启停沿用 TogglePlugin 的授权语义）
4. 可回滚：update.sh 保留上一版二进制（`.prev`），失败时 systemd restart
   自动回退（记录在 update-status）

## 协议扩展（echo-protocol）

```rust
BackendCommand::RequestPluginsList       // → BackendEvent::PluginsList
BackendCommand::TogglePlugin { id, enabled }
BackendEvent::PluginsList { plugins: Vec<PluginInfo> }
PluginInfo { id, name, version, kind, enabled, builtin, description }
```

## Panel

新增"插件"视图（CapabilitiesPanel 扩展或独立 PluginPanel）：
- 按 kind 分组的二级菜单
- 每项：id/版本/描述/启用开关（TogglePlugin）
- 自更新状态卡：revision / 插件摘要（framework_update status 视图）

## 边界与不做

- 不做动态库加载（ABI 不稳定，见上）
- 不做 plugin 沙箱（源码级插件即本仓库内代码，信任边界 = 仓库）
- 不做插件市场/远程安装（Phase 2+，需签名与供应链考虑）

## 兼容性

- 现有 config 字段全部保留（skills_dir 等）；新增字段均带默认值
- 旧版本二进制可读新配置（serde default）
- Panel 旧版本忽略未知事件（前端 reducer 按 key 分派，未知键无操作）
