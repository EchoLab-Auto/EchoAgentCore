//! 启动封装：把 `plugins.toml` 合成出的插件树翻译成宿主启动动作
//! （解耦计划 P3/P4）。
//!
//! 职责划分：
//! - [`launch_specs`]：从 [`echo_plugin_loader::LoadedTree`] 中筛选
//!   `enabled == true` 且 `kind == Stdio` 的条目，转成 [`LaunchSpec`]；
//!   builtin / dylib / wasm（非本运输层）与 disabled 条目连同原因进入第二个
//!   返回值（人可读，供日志 / 诊断）。
//! - [`start_plugins`]：**best-effort** 逐个启动——单个插件失败（spawn 失败 /
//!   握手超时等）不阻断其余插件，失败记入 [`StartedPlugins::errors`]。
//! - [`start_from_file`]：`load(path)` + [`start_plugins`] 一步到位。
//! - [`StartedPlugins::tools`] / [`StartedPlugins::shutdown`]：汇总远程工具与
//!   统一排空。
//!
//! 语义要点：
//! - `config` 原样透传（[`PluginSpec::config`] → `Hello.config`）；
//! - `env` 逐条注入（stdio 运输层默认不继承宿主环境，仅透传 `PATH`）；
//! - shutdown 对每个句柄 `drain(deadline)`，单点错误只记 debug 日志。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use echo_plugin_loader::{EntryKind, LoadedTree};

use crate::stdio::StdioTransport;
use crate::supervisor::{PluginHandle, PluginSupervisor};
use crate::tool::RemoteTool;
use crate::transport::PluginSpec;

/// 一个可启动的 stdio 插件实例（由 `plugins.toml` 的一个条目翻译而来）。
#[derive(Debug, Clone)]
pub struct LaunchSpec {
    /// 宿主侧插件实例 id（Hello/Welcome 握手比对；同一棵树内唯一）。
    pub plugin_id: String,
    /// 可执行文件路径（loader 已校验非空）。
    pub command: String,
    /// 追加命令行参数。
    pub args: Vec<String>,
    /// 注入子进程的环境变量（宿主环境默认不继承，仅透传 `PATH`）。
    pub env: BTreeMap<String, String>,
    /// 插件配置（原样进 [`PluginSpec::config`]）。
    pub config: Value,
}

/// 从合成树中筛选可启动的 stdio 实例。
///
/// 返回 `(specs, skipped)`：`skipped` 是跳过原因（人可读），形如
/// `"websearch: kind builtin skipped"` / `"x: disabled"` /
/// `"y: stdio without command skipped"`。
///
/// stdio 条目缺 `command` 不应出现（loader 已校验），如遇手工构造的树
/// 则跳过并记录。
pub fn launch_specs(tree: &LoadedTree) -> (Vec<LaunchSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut skipped = Vec::new();
    for entry in &tree.entries {
        if !entry.enabled {
            skipped.push(format!("{}: disabled", entry.id));
            continue;
        }
        if entry.kind != EntryKind::Stdio {
            skipped.push(format!(
                "{}: kind {} skipped",
                entry.id,
                entry.kind.as_str()
            ));
            continue;
        }
        let Some(command) = entry.command.as_deref().filter(|c| !c.trim().is_empty()) else {
            // 防御性分支：loader 校验过的树不会走到这里。
            skipped.push(format!("{}: stdio without command skipped", entry.id));
            continue;
        };
        specs.push(LaunchSpec {
            plugin_id: entry.id.clone(),
            command: command.to_string(),
            args: entry.args.clone(),
            env: entry.env.clone(),
            config: entry.config.clone(),
        });
    }
    (specs, skipped)
}

/// 启动结果：成功握手的句柄 + 单点失败记录。
pub struct StartedPlugins {
    /// 成功完成握手的插件句柄（顺序与 `specs` 一致，失败的被跳过）。
    pub handles: Vec<PluginHandle>,
    /// 启动失败的插件：`(plugin_id, 错误描述)`。
    pub errors: Vec<(String, String)>,
}

impl StartedPlugins {
    /// 汇总全部句柄的工具贡献（每个插件工具一个 [`RemoteTool`] 适配器；
    /// 未注册工具贡献的插件贡献空表）。
    pub fn tools(&self) -> Vec<Arc<RemoteTool>> {
        self.handles.iter().flat_map(|h| h.remote_tools()).collect()
    }

    /// 逐个排空（`drain(deadline)`）；单点错误只记 debug 日志，不影响其余。
    pub async fn shutdown(&self, deadline: Duration) {
        for handle in &self.handles {
            if let Err(error) = handle.drain(deadline).await {
                tracing::debug!(
                    plugin = %handle.plugin_id(),
                    %error,
                    "plugin drain failed"
                );
            }
        }
    }
}

/// 逐个启动（best-effort）：`start` 失败记录到 [`StartedPlugins::errors`]
/// 并继续启动其余插件。
pub async fn start_plugins(
    supervisor: &PluginSupervisor,
    specs: Vec<LaunchSpec>,
) -> StartedPlugins {
    let mut handles = Vec::new();
    let mut errors = Vec::new();
    for spec in specs {
        let mut transport = StdioTransport::new(&spec.command).with_args(&spec.args);
        for (key, value) in &spec.env {
            transport = transport.with_env(key, value);
        }
        let plugin_spec = PluginSpec {
            plugin_id: spec.plugin_id.clone(),
            config: spec.config.clone(),
        };
        match supervisor.start(plugin_spec, Box::new(transport)).await {
            Ok(handle) => {
                tracing::info!(plugin = %handle.plugin_id(), "plugin started");
                handles.push(handle);
            }
            Err(error) => {
                tracing::warn!(plugin = %spec.plugin_id, %error, "plugin start failed");
                errors.push((spec.plugin_id, error.to_string()));
            }
        }
    }
    StartedPlugins { handles, errors }
}

/// `load(path)` 失败原样返回 [`echo_plugin_loader::LoadError`]；成功则
/// best-effort [`start_plugins`]。
pub async fn start_from_file(
    path: &Path,
    supervisor: &PluginSupervisor,
) -> Result<StartedPlugins, echo_plugin_loader::LoadError> {
    let tree = echo_plugin_loader::load(path)?;
    let (specs, _skipped) = launch_specs(&tree);
    Ok(start_plugins(supervisor, specs).await)
}
