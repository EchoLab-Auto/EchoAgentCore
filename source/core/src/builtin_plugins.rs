//! 内置插件的**目录**（唯一归属）。
//!
//! ## 这个文件回答什么问题
//!
//! 「这个二进制里有哪些内置插件、各自的 `mount` 做什么」。此前这 378 行散在
//! `main.rs` 的八个「实化 N」段里，而 `plugins.toml` 决定的是**装哪些、什么
//! 顺序**——两者放在一起看，很难回答"新增一个内置插件要改哪里"。
//!
//! ## 与配置的关系（这是 dsh 的形态）
//!
//! ```text
//!   builtin_plugins::catalog()   ← 本文件：代码里的插件目录（构造 + mount）
//!              ↓
//!   plugins.toml 的 builtin 行   ← 配置：顺序、启用状态、每插件的 config
//!              ↓
//!   PluginHost::mount_plan_when_ready  ← 挂载：等依赖就绪，失败即启动失败
//! ```
//!
//! 构造留在代码里是**刻意的**：`mount` 闭包必须捕获运行期句柄（端口 / 令牌 /
//! 实例句柄 / broker），而那些东西在配置里表达不了。配置能表达的是"装不装、
//! 哪个先装、给它的那份 config 长什么样"——三者都在 `plugins.toml`。
//!
//! ## 迁移方向（全插件化 S3 的剩余工作）
//!
//! 本文件里的条目大多是**门控壳**：`mount` 只做 `reapply_plugin_gating`，
//! 因为真实实现已经在对应插件 crate 里（`echo-plugin-tools` / `-skills` /
//! `-subagent` / `-workspace` / `-federation` / `-management`）。壳之所以还在
//! 这里，是因为**构造参数是宿主的东西**（哪个人格、哪个端口、哪个事件出口）。
//! 要彻底消除它们，需要「插件自己声明构造参数」的能力（框架的 `factory`
//! 注册表）——那是下一步，不是这一轮。

use std::sync::Arc;

use echo_context::Disposer;
use echo_plugin::{BuiltinPlugin, MountContext, Plugin, PluginManifest, PluginRuntime};

/// 插件 id 的**形状**校验（装配 bug 早失败胜过面板里出现怪条目）。
///
/// 真实踩到过的一个：没配置 `[instances.<平台>]` 时，实例插件的 id 会被派生成
/// `echo-agent.instance.`（尾随点、平台名为空）——它在 `plugins mounted` 日志
/// 与面板清单里都是一个点不动的幽灵插件，而当时没有任何断言拦得住。
fn validate_plugin_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() {
        return Err(anyhow::anyhow!("builtin plugin id must not be empty"));
    }
    let last = id.rsplit('.').next().unwrap_or("");
    if last.is_empty() {
        // 派生 id 的空段几乎一定是"平台名/名字没取到"的装配 bug。
        return Err(anyhow::anyhow!(
            "builtin plugin id {id:?} ends with an empty segment (derived name missing?)"
        ));
    }
    Ok(())
}

/// 压入一个只有 manifest + mount 闭包的插件。
fn push_builtin(
    catalog: &mut Vec<Arc<dyn Plugin>>,
    manifest: PluginManifest,
    mount: impl Fn(&MountContext) -> echo_plugin::PluginMountResult + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let id = manifest.id.clone();
    validate_plugin_id(&id)?;
    if catalog.iter().any(|p| p.manifest().id == id) {
        // 装配 bug（同 id 两条构造）——早失败胜过静默覆盖。
        return Err(anyhow::anyhow!("duplicate builtin plugin id {id}"));
    }
    catalog.push(Arc::new(BuiltinPlugin::new(manifest, mount)));
    Ok(())
}

/// 压入一个**已构造好**的插件（带贡献：命令 / 文本块 / 事件…）。
///
/// 与 [`push_builtin`] 的分工：后者只给 manifest + mount 闭包，构造器的额外
/// 能力（`with_commands` / `with_text_blocks` / …）用不上。
fn push_plugin(catalog: &mut Vec<Arc<dyn Plugin>>, plugin: Arc<dyn Plugin>) -> anyhow::Result<()> {
    let id = plugin.manifest().id.clone();
    validate_plugin_id(&id)?;
    if catalog.iter().any(|p| p.manifest().id == id) {
        return Err(anyhow::anyhow!("duplicate builtin plugin id {id}"));
    }
    catalog.push(plugin);
    Ok(())
}

/// 构造**全部内置插件**（顺序即目录顺序；实际挂载顺序由 `plugins.toml` 定）。
///
/// # 零参数（全插件化 S6）
///
/// 这个签名此前是 `catalog(version, hosts: &Hosts)`，`Hosts` 有 13 个字段，
/// 每一项都是组合根手里的运行期句柄（persona 管理器 / 事件出口 / 联邦端口 /
/// 实例句柄 / 管理面桥 / 端口与令牌……）。问题是：**那些句柄的去向是插件的
/// 私事**，写在组合根里等于让"新增一个插件"要改三处（目录 / 参数组装 /
/// 门控闭包），而插件自己反而不声明它需要什么。
///
/// 现在每个插件在 `mount` 里按自己声明的服务键解析（`MountContext::require`），
/// 组合根只负责把句柄注册进服务表（`host_services::register`）。于是：
///
/// ```text
///   catalog(version)          ← 本函数：目录。谁存在、按什么顺序
///   host_services::register   ← 宿主：我有什么能力，按什么键
///   插件 mount: require(key)  ← 插件：我需要什么
/// ```
///
/// **配置驱动的那部分不在目录里**：平台实例插件（id 由 `[instances.<平台>]`
/// 派生）与外部插件条目（`plugins.toml` 的行）由调用点在目录之后追加——它们
/// 的"存在性"取决于这份部署的配置，而不是这个二进制里有什么。
pub(crate) fn catalog(version: &str) -> anyhow::Result<Vec<Arc<dyn Plugin>>> {
    let mut catalog: Vec<Arc<dyn Plugin>> = Vec::new();

    // ── 内置工具集（工具清单所有权在插件里）──
    push_plugin(
        &mut catalog,
        echo_agent::tool::builtin::builtin_plugin::plugin(version),
    )?;

    // ── 技能目录（整表 + 命令域）──
    push_plugin(&mut catalog, echo_agent::skill::plugin(version))?;

    // ── 循环模式（二选一，纯策略）──
    //
    // 驱动本体只有一个（echo-loop TurnRunner，2026-10 单轨化），两个模式插件
    // 只表达策略（per-persona 白名单推导的 `LoopMode`）；mount/unmount 不切换
    // 驱动、不注册任何运行期资源。
    //
    // 协议化：插件族声明 `Service{key:"loop"}` 贡献；runner 实例由插件在 mount
    // 里构造并 provide，服务键与贡献声明同源。
    for plugin in echo_plugin_loop::plugins(version) {
        push_plugin(&mut catalog, plugin)?;
    }

    // ── 工作区会话（命令域 + 提示词块）──
    push_plugin(&mut catalog, echo_plugin_workspace::plugin(version))?;

    // ── 联邦（命令域 + 远程工具路由）──
    push_plugin(&mut catalog, echo_plugin_federation::plugin(version))?;

    // ── subagent 委派（纯身份：工具与 hook 在 echo-agent 侧实现）──
    push_builtin_identity(&mut catalog, version, echo_agent::plugins::SUBAGENT_PLUGIN_ID)?;

    // ── 管理面（Panel 通道 + 会话视图命令域）──
    push_plugin(&mut catalog, echo_plugin_management::plugin(version))?;

    // ── LLM provider（工厂 + 池规则 + API 命令域）──
    push_plugin(&mut catalog, echo_plugin_provider_llm::plugin(version))?;

    // ── 自更新（updater 单元 + 状态文件）──
    push_plugin(&mut catalog, echo_plugin_self_update::plugin(version))?;

    reject_duplicate_commands(&catalog)?;
    reject_reserved_commands(&catalog)?;
    Ok(catalog)
}

/// 平台实例插件（id 由 `[instances.<平台>]` 的段名派生）。
///
/// 它是**门控 + 启停壳**：实现由独立进程（`echo-adapter-<platform>`）提供。
/// 参数只剩平台名（配置事实）；启停控制从 [`crate::host_services::INSTANCE_CONTROL`]
/// 服务键解析——那是宿主的运行期句柄，不该出现在目录签名里。
pub(crate) fn instance_plugin(version: &str, platform: &str) -> Arc<dyn Plugin> {
    
    let plugin_id = crate::instance_manifests::plugin_id_for(platform);
    let name = format!("{platform} 插件实例");
    let description = format!("{platform} 平台实例插件（独立进程）");
    let plugin_id_for_mount = plugin_id.clone();
    Arc::new(
        BuiltinPlugin::new(
            PluginManifest::builtin(&plugin_id, &name, version, "instance", platform, description)
                // 实例插件化收尾（P1.9，2026-10）：实现由独立进程提供，这里只是
                // 门控壳。必须显式声明，否则面板按 builtin=true 把它显示成
                // 「内部插件」。
                .with_runtime(PluginRuntime::Subprocess),
            move |mctx| {
                // 启停控制是可选的：没有实例句柄的部署（不跑平台）照样能挂上。
                let control = mctx.service(&crate::host_services::INSTANCE_CONTROL);
                if let Some(control) = &control {
                    if control.is_wired() {
                        let control = Arc::clone(control);
                        tokio::spawn(async move { control.start().await });
                    }
                }
                let plugin_id = plugin_id_for_mount.clone();
                Ok(vec![Disposer::from_fn(move || {
                    // 卸载：停实例（门控由宿主的挂载/卸载事件统一拨）。
                    if let Some(control) = &control {
                        let control = Arc::clone(control);
                        tokio::spawn(async move { control.stop().await });
                    }
                    let _ = &plugin_id;
                })])
            },
        ),
    )
}

/// 外部插件条目（`plugins.toml` 的非 builtin 行）的**名单壳**。
///
/// 子进程由 supervisor 拉起，这里的 mount 是**名义挂载**——它让条目出现在面板
/// 的插件清单里（此前用户装的插件在清单里根本看不到）。禁用要重启 Core 生效，
/// 重启按钮对这类条目回「请重启 Core」。
pub(crate) fn external_plugin(
    version: &str,
    entry: &ExternalEntry,
) -> Arc<dyn Plugin> {
    Arc::new(BuiltinPlugin::new(
        // `builtin = false`：来源维度上它不是编译进本二进制的（面板的来源徽标
        // 读这个字段，分组读 `runtime`——两者正交，别互相顶替）。
        PluginManifest::builtin(
            &entry.id,
            &entry.name,
            version,
            "tool",
            &entry.id,
            format!("外部插件（plugins.toml，{}）", entry.kind),
        )
        .with_runtime(PluginRuntime::Subprocess)
        .with_builtin_flag(false),
        |_ctx| Ok(vec![]),
    ))
}

/// 一个外部插件条目（`plugins.toml` 的 `kind != "builtin"` 行）。
pub(crate) struct ExternalEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// 压入一个**只有身份**的内置插件（tool / hook 实现在宿主侧，见该插件文档）。
fn push_builtin_identity(
    catalog: &mut Vec<Arc<dyn Plugin>>,
    version: &str,
    id: &str,
) -> anyhow::Result<()> {
    let (name, kind, route, description) = match id {
        echo_agent::plugins::SUBAGENT_PLUGIN_ID => (
            "Subagent 委派",
            "tool",
            "subagent",
            "委派独立子任务给隔离上下文的子 agent：spawn_subagent 工具 + 完成后 <subagent_event> hook 回灌主 agent",
        ),
        _ => (id, "tool", id, "内置插件"),
    };
    push_builtin(
        catalog,
        PluginManifest::builtin(id, name, version, kind, route, description),
        |_ctx| Ok(vec![]),
    )
}

/// 声明了**宿主保留命令**的插件是死代码：命令泵在分发前就把它挡掉了
/// （`command_is_host_reserved`），插件永远收不到，且**没有任何提示**。
///
/// 这正是 `RequestSelfUpdate` / `MigrateSession` 曾经的形状——保留在名单里
/// + 实现只在插件侧 = 必失败。装配期报出来，这类"声明了但收不到"就不可能
/// 再溜进去。
fn reject_reserved_commands(catalog: &[Arc<dyn Plugin>]) -> anyhow::Result<()> {
    for plugin in catalog {
        let id = plugin.manifest().id.clone();
        for command in plugin.handled_commands() {
            if echo_agent::plugins::command_is_host_reserved(&command) {
                return Err(anyhow::anyhow!(
                    "{id} declares host-reserved command {command}: the dispatcher will never \
                     route it to the plugin (remove the declaration or the reservation)"
                ));
            }
        }
    }
    Ok(())
}

/// 目录形状的**可执行守卫**（全插件化 S6 的回归防线）。
///
/// 插件零捕获之后，"我需要什么"写在插件的 `inject()` 里，"我能给什么"写在
/// 宿主的 [`crate::host_services::register`] 里。两者跨 crate、跨文件，编译器
/// 看不见——对不上的表现是**启动期挂载失败**（`MissingService`）。这组测试
/// 把它提前到 `cargo test`。
#[cfg(test)]
mod catalog_shape_tests {
    use super::*;

    /// 每个插件声明的硬依赖，宿主都得真的会注册。
    ///
    /// 反向不检查（宿主注册了但没人用的键是允许的：那是"将来的插件"的接缝，
    /// 也是诊断面）。
    #[test]
    fn every_declared_dependency_is_registered_by_the_host() {
        let provided = crate::host_services::provided_keys();
        let catalog = catalog("0.0.0-test").expect("目录构造");
        for plugin in &catalog {
            let id = plugin.manifest().id.clone();
            for key in plugin.inject().required {
                assert!(
                    provided.contains(&key),
                    "插件 {id} 声明硬依赖 `{key}`，但宿主没有注册这个键。\n\
                     能注册的键：{provided:?}\n\
                     → 在 `host_services::register` 里加一行，或从插件的 inject 里去掉"
                );
            }
        }
    }

    /// `llm` 键的 ownership 不得再漂移（2026-11 S10 的更正）。
    ///
    /// 这条守着一段真实的历史误解：`echo-plugin-provider-llm` 里曾有个叫
    /// `SERVICE_KEY` 的常量，值是 `"llm"`，文档写"本插件贡献的服务键"——而
    /// 它**并不提供**这个键（提供的是命令域接缝 `PROVIDER_HOST`）。真正的
    /// 注册方是组合根，消费方是 `loop` 插件。于是"谁拥有 `llm`"读代码会读错。
    ///
    /// 现在键住在定义层（`echo_defs::llm::LLM_PROVIDER_SERVICE_KEY`），三方
    /// 都引用它。这条测试锁住三件事：
    /// ① 宿主声明会提供它；② 消费方（loop）确实声明依赖它；
    /// ③ 没有任何内置插件"认领"它是自己 provide 的（名义 ownership 归宿主）。
    #[test]
    fn llm_service_key_has_exactly_one_owner() {
        let key = echo_defs::llm::LLM_PROVIDER_SERVICE_KEY;
        assert!(
            crate::host_services::provided_keys().contains(&key.to_string()),
            "宿主必须声明提供 {key}（组合根按配置构建默认 provider）"
        );

        let catalog = catalog("0.0.0-test").expect("目录构造");
        let loop_depends = catalog.iter().any(|plugin| {
            plugin.manifest().kind == "loop" && plugin.inject().required.iter().any(|k| k == key)
        });
        assert!(loop_depends, "loop 插件必须声明依赖 {key}");

        // ③ 插件侧的**别名**必须指向定义层（不是又写一遍字面量）。
        assert_eq!(
            echo_plugin_provider_llm::PROVIDER_KEY,
            key,
            "provider 插件的别名必须指向定义层常量，不能自己再写一遍 \"llm\""
        );

        // ④ 源码里不许再出现那两种历史形状（各自独立的一份字面量）。
        //    这是**真的**防回归：行为对、名字错的漂移只有源码能抓到。
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("repo root")
            .join("source");
        let banned = ["SERVICE_KEY: &str = \"llm\"", "Inject::new([\"llm\"])"];
        // 定义层自己就是那份字面量的家；注释里提到旧形状是**说明**不是副本。
        let definition = root.join("defs/echo-defs/src/llm.rs");
        let mut hits = Vec::new();
        walk_rs(&root, &mut |path, text| {
            if path == definition {
                return;
            }
            for (index, line) in text.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                for pattern in banned {
                    if line.contains(pattern) {
                        hits.push(format!("{}:{}: {pattern}", path.display(), index + 1));
                    }
                }
            }
        });
        assert!(
            hits.is_empty(),
            "`llm` 键又出现了独立副本（应引用 echo_defs::llm::LLM_PROVIDER_SERVICE_KEY）：{hits:#?}"
        );
    }

    /// 递归遍历 `.rs`（不引第三方 walkdir：这一段只在测试里跑）。
    fn walk_rs(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path, &str)) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                    continue;
                }
                walk_rs(&path, f);
            } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    f(&path, &text);
                }
            }
        }
    }

    /// 孤儿命令：插件声明要接管的命令，若没有任何插件认领，是**静默死路**
    /// （面板点了没反应）。反向由各插件自己的"声明 ⇒ 认领"测试守。
    #[test]
    fn no_command_is_declared_twice_across_the_catalog() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        reject_duplicate_commands(&catalog).expect("命令不得被两个插件同时声明");
    }

    /// 目录里不许有幽灵条目：id 的空段（`echo-agent.instance.`）是派生 bug。
    #[test]
    fn catalog_ids_are_well_formed() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        for plugin in &catalog {
            validate_plugin_id(&plugin.manifest().id).expect("id 形状");
        }
    }

    /// **静态目录不含配置驱动项**：平台实例插件与外部的 `plugins.toml` 条目
    /// 由调用点追加（它们的"存在"取决于这份部署，不是二进制里有什么）。
    #[test]
    fn static_catalog_has_no_deployment_specific_entries() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        for plugin in &catalog {
            let id = plugin.manifest().id.clone();
            assert!(
                !id.starts_with(crate::instance_manifests::PLUGIN_ID_PREFIX),
                "平台实例插件不该出现在静态目录里：{id}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 派生 id 的空段必须被拦下（真实踩到过：无 `[instances]` 配置时出现
    /// 一个叫 `echo-agent.instance.` 的幽灵插件）。
    #[test]
    fn plugin_id_shape_is_validated() {
        assert!(validate_plugin_id("echo-agent.instance.qq").is_ok());
        assert!(validate_plugin_id("echo-agent.tools.builtin").is_ok());
        assert!(validate_plugin_id("").is_err());
        assert!(validate_plugin_id("echo-agent.instance.").is_err());
        // 中间的空段是既有 id 形态（`<prefix>.<name>` 之外没人管），不拦。
        assert!(validate_plugin_id("echo-agent..x").is_ok());
    }

    /// 同一条命令被两个插件声明 → 装配期失败（否则先挂上的静默赢）。
    #[test]
    fn duplicate_command_declarations_are_rejected() {
        let one = Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("a.one", "a", "0", "tool", "a", "a"),
            |_| Ok(vec![]),
        )
        .with_commands(["Shared"]));
        let two = Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("a.two", "b", "0", "tool", "b", "b"),
            |_| Ok(vec![]),
        )
        .with_commands(["Shared"]));
        let err = reject_duplicate_commands(&[one, two]).expect_err("重名必须报错");
        assert!(err.to_string().contains("Shared"), "{err}");
    }

    /// 声明宿主保留命令 → 装配期失败（分发器永远不会路由给它）。
    #[test]
    fn reserved_command_declarations_are_rejected() {
        let plugin = Arc::new(BuiltinPlugin::new(
            PluginManifest::builtin("a.bad", "bad", "0", "tool", "bad", "bad"),
            |_| Ok(vec![]),
        )
        .with_commands(["TogglePlugin"]));
        let err = reject_reserved_commands(&[plugin]).expect_err("保留命令必须报错");
        assert!(err.to_string().contains("TogglePlugin"), "{err}");
    }
}

/// 同一条命令被两个内置插件声明 = **静默遮蔽**（先挂上的那个赢，另一个永远
/// 收不到）——这是注册关系层面的装配 bug，启动期就报出来。
///
/// 为什么要在这里查：命令名与归属都是各插件 crate 自己声明的（这正是 S5 的
/// 成果），框架分发时只是"问一遍谁认领"。跨 crate 的重名没有任何编译器守得住。
fn reject_duplicate_commands(catalog: &[Arc<dyn Plugin>]) -> anyhow::Result<()> {
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for plugin in catalog {
        let id = plugin.manifest().id.clone();
        for command in plugin.handled_commands() {
            if let Some(previous) = seen.insert(command.clone(), id.clone()) {
                return Err(anyhow::anyhow!(
                    "command {command} is claimed by both {previous} and {id}"
                ));
            }
        }
    }
    Ok(())
}
