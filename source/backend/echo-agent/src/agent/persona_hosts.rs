//! persona 作用域的**宿主适配器供给**（2026-11 S10：对齐 `ctx.fork()`）。
//!
//! ## 这里解决什么
//!
//! 命令域归插件之后，插件从服务表里解析"宿主能力"（`ShellCommandHost` /
//! `ToolsCommandHost` / `SkillsCommandHost` / `ProviderCommandHost`）。这些能力
//! 里的每一样都**属于某个人格**：
//!
//! | 能力 | 为什么必须按人格 |
//! |---|---|
//! | 工具启停 | 每个 persona 有自己的 `ToolRegistry` |
//! | 技能表 | 每个 persona 有自己的 `SkillRegistry` |
//! | 模型 / provider | 每个 persona 有自己的 `ModelService`（persona API） |
//! | Shell 会话归属 | 会话记录里的 `team_id` 来自执行者 |
//!
//! 它们此前一律绑到**进程级核心服务代理**（`core_agent`，team_id 为空）：
//! 命令能跑，但"作用于谁"是错的——`ToggleTool` 落到 core_agent 的表上，再靠
//! 一段"逐 persona 重算"的 fan-out 补回来。那是**结果对的巧合**，不是设计。
//!
//! ## 做成什么
//!
//! 每个人格有一个**自己的 `Ctx`**（`root.child()`，父链继承宿主服务），把上面
//! 四个适配器按人格提供进去。persona 作用域的插件（`kind ∈ {tool, skill,
//! provider}`）挂在这一层，于是它的 `ServiceSlot` 绑定到**自己那个人格**的
//! 适配器。命令经这一层分发时，"执行者 = 归属者"由**作用域**保证，而不是由
//! 谁恰好持有哪个 `Arc` 决定。
//!
//! ## 为什么不是"给适配器加 team_id 参数"
//!
//! 那会把人格身份塞进每一条命令处理的参数里（且要改全部 trait 签名）。作用域
//! 是**上下文**，上下文本来就该在环境里而不是在参数里——这正是 `ctx.fork()`
//! 的语义。

use std::sync::Arc;

use echo_context::Ctx;
use echo_plugin_provider_llm::commands::ProviderCommandHost;
use echo_plugin_skills::commands::SkillsCommandHost;
use echo_plugin_tools::shell_commands::ShellCommandHost;
use echo_plugin_tools::tool_commands::ToolsCommandHost;

use crate::agent::provider_host::AgentProviderCommandHost;
use crate::agent::shell_host::AgentShellCommandHost;
use crate::agent::skills_host::AgentSkillsCommandHost;
use crate::agent::tools_host::AgentToolsCommandHost;
use crate::agent::Agent;

/// 把"这个人格的四项宿主能力"提供进它的 `Ctx`（子作用域）。
///
/// 返回的 `Disposer` 必须活到作用域销毁——它们撤掉的是本层的服务登记
/// （作用域一起销毁时这一步是空转，但保持 RAII 纪律：谁 provide 谁持有）。
///
/// 未提供 / 提供失败的情形一律记 warn 并继续：缺一项能力只让对应命令域
/// 报"插件尚未挂载"，不该让整个人格的作用域建不起来。
pub(crate) fn provide(ctx: &Arc<Ctx>, agent: &Arc<Agent>) -> Vec<echo_context::Disposer> {
    let mut keep = Vec::new();
    fn put<T: Send + Sync + 'static>(
        keep: &mut Vec<echo_context::Disposer>,
        ctx: &Arc<Ctx>,
        key: &echo_context::ServiceKey<T>,
        value: T,
    ) {
        match ctx.provide(key, value) {
            Ok(disposer) => keep.push(disposer),
            Err(error) => tracing::warn!(
                team_id = key.name(),
                %error,
                "persona host service already provided"
            ),
        }
    }

    put(
        keep.as_mut(),
        ctx,
        &echo_plugin_tools::host_keys::SHELL_HOST,
        Arc::new(AgentShellCommandHost::new(agent.clone())) as Arc<dyn ShellCommandHost>,
    );
    put(
        keep.as_mut(),
        ctx,
        &echo_plugin_tools::host_keys::TOOLS_HOST,
        Arc::new(AgentToolsCommandHost::new(agent.clone())) as Arc<dyn ToolsCommandHost>,
    );
    put(
        keep.as_mut(),
        ctx,
        &echo_plugin_skills::host_keys::SKILLS_HOST,
        Arc::new(AgentSkillsCommandHost::new(agent.clone())) as Arc<dyn SkillsCommandHost>,
    );
    put(
        keep.as_mut(),
        ctx,
        &echo_plugin_provider_llm::host_keys::PROVIDER_HOST,
        Arc::new(AgentProviderCommandHost::new(agent.clone())) as Arc<dyn ProviderCommandHost>,
    );
    keep
}
