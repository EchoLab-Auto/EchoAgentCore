//! 内置模块的**目录**（唯一归属）。
//!
//! ## 这个文件回答什么问题
//!
//! 「这个二进制里有哪些内置模块、各自的 `mount` 做什么」。此前这 378 行散在
//! `main.rs` 的八个「实化 N」段里，而 `modules.toml` 决定的是**装哪些、什么
//! 顺序**——两者放在一起看，很难回答"新增一个内置模块要改哪里"。
//!
//! ## 与配置的关系（这是 dsh 的形态）
//!
//! ```text
//!   builtin_modules::catalog_with()   ← 本文件：代码里的模块目录（构造 + mount）
//!              ↓
//!   modules.toml 的 builtin 行   ← 配置：顺序、启用状态、每模块的 config
//!              ↓
//!   ModuleHost::mount_plan_when_ready  ← 挂载：等依赖就绪，失败即启动失败
//! ```
//!
//! 构造留在代码里是**刻意的**：`mount` 闭包必须捕获运行期句柄（端口 / 令牌 /
//! 实例句柄 / broker），而那些东西在配置里表达不了。配置能表达的是"装不装、
//! 哪个先装、给它的那份 config 长什么样"——三者都在 `modules.toml`。
//!
//! ## 迁移方向（全模块化 S3 的剩余工作）
//!
//! 本文件里的条目大多是**门控壳**：`mount` 只做 `reapply_module_gating`，
//! 因为真实实现已经在对应模块 crate 里（`echo-module-tools` / `-skills` /
//! `-subagent` / `-workspace` / `-federation` / `-management`）。壳之所以还在
//! 这里，是因为**构造参数是宿主的东西**（哪个人格、哪个端口、哪个事件出口）。
//! 要彻底消除它们，需要「模块自己声明构造参数」的能力（框架的 `factory`
//! 注册表）——那是下一步，不是这一轮。

use std::sync::Arc;

use echo_context::Disposer;
use echo_module::{BuiltinModule, Module, ModuleManifest, ModuleRuntime, MountContext};

/// 模块 id 的**形状**校验（装配 bug 早失败胜过面板里出现怪条目）。
///
/// 真实踩到过的一个：没配置 `[instances.<平台>]` 时，实例模块的 id 会被派生成
/// `echo-agent.instance.`（尾随点、平台名为空）——它在 `modules mounted` 日志
/// 与面板清单里都是一个点不动的幽灵模块，而当时没有任何断言拦得住。
fn validate_module_id(id: &str) -> anyhow::Result<()> {
    if id.is_empty() {
        return Err(anyhow::anyhow!("builtin module id must not be empty"));
    }
    let last = id.rsplit('.').next().unwrap_or("");
    if last.is_empty() {
        // 派生 id 的空段几乎一定是"平台名/名字没取到"的装配 bug。
        return Err(anyhow::anyhow!(
            "builtin module id {id:?} ends with an empty segment (derived name missing?)"
        ));
    }
    Ok(())
}

/// 压入一个只有 manifest + mount 闭包的模块。
fn push_builtin(
    catalog: &mut Vec<Arc<dyn Module>>,
    manifest: ModuleManifest,
    mount: impl Fn(&MountContext) -> echo_module::ModuleMountResult + Send + Sync + 'static,
) -> anyhow::Result<()> {
    let id = manifest.id.clone();
    validate_module_id(&id)?;
    if catalog.iter().any(|p| p.manifest().id == id) {
        // 装配 bug（同 id 两条构造）——早失败胜过静默覆盖。
        return Err(anyhow::anyhow!("duplicate builtin module id {id}"));
    }
    catalog.push(Arc::new(BuiltinModule::new(manifest, mount)));
    Ok(())
}

/// 压入一个**已构造好**的模块（带贡献：命令 / 文本块 / 事件…）。
///
/// 与 [`push_builtin`] 的分工：后者只给 manifest + mount 闭包，构造器的额外
/// 能力（`with_commands` / `with_text_blocks` / …）用不上。
fn push_module(catalog: &mut Vec<Arc<dyn Module>>, module: Arc<dyn Module>) -> anyhow::Result<()> {
    let id = module.manifest().id.clone();
    validate_module_id(&id)?;
    if catalog.iter().any(|p| p.manifest().id == id) {
        return Err(anyhow::anyhow!("duplicate builtin module id {id}"));
    }
    catalog.push(module);
    Ok(())
}

/// 构造**全部内置模块**（顺序即目录顺序；实际挂载顺序由 `modules.toml` 定）。
///
/// # 零参数（全模块化 S6）
///
/// 这个签名此前是 `catalog(version, hosts: &Hosts)`，`Hosts` 有 13 个字段，
/// 每一项都是组合根手里的运行期句柄（persona 管理器 / 事件出口 / 联邦端口 /
/// 实例句柄 / 管理面桥 / 端口与令牌……）。问题是：**那些句柄的去向是模块的
/// 私事**，写在组合根里等于让"新增一个模块"要改三处（目录 / 参数组装 /
/// 门控闭包），而模块自己反而不声明它需要什么。
///
/// 现在每个模块在 `mount` 里按自己声明的服务键解析（`MountContext::require`），
/// 组合根只负责把句柄注册进服务表（`host_services::register`）。于是：
///
/// ```text
///   catalog(version)          ← 本函数：目录。谁存在、按什么顺序
///   host_services::register   ← 宿主：我有什么能力，按什么键
///   模块 mount: require(key)  ← 模块：我需要什么
/// ```
///
/// **配置驱动的那部分不在目录里**：平台实例模块（id 由 `[instances.<平台>]`
/// 派生）与外部模块条目（`modules.toml` 的行）由调用点在目录之后追加——它们
/// 的"存在性"取决于这份部署的配置，而不是这个二进制里有什么。
#[cfg(test)]
pub(crate) fn catalog(version: &str) -> anyhow::Result<Vec<Arc<dyn Module>>> {
    catalog_with(version, std::iter::empty())
}

/// 组合根可以把来自**任意 crate** 的内置模块附加到目录。
///
/// 这是外部仓进程内模块的唯一装配接缝：实现、配置与生命周期全留在那个
/// crate；本目录不需要知道它的领域、ID 或挂载细节。空迭代器保持原目录。
pub(crate) fn catalog_with(
    version: &str,
    additional: impl IntoIterator<Item = Arc<dyn Module>>,
) -> anyhow::Result<Vec<Arc<dyn Module>>> {
    let mut catalog: Vec<Arc<dyn Module>> = Vec::new();

    // ── 运行地基（`kind = "kernel"`）──
    //
    // 2026-11：取消「中间层 / 模块」二分。这 8 个此前是"只是库"的 crate，
    // 现在与其它模块同形（有 manifest、进 Panel 清单、可 mount/unmount）。
    // 排最前是因为它们提供的是**其它模块的地基**——先登记身份，读配置时
    // 就能看到完整清单。
    //
    // **`mount` 是纯身份声明**（空 disposer）：地基的运行期实例（工具注册表 /
    // 事件日志 / Shell 管理器…）有归属问题（每 persona 一份，或要绑定宿主
    // 事件出口），由组合根构造后经服务键交给消费者，不在 mount 里申请。
    //
    // `echo-defs` 与 `echo-config` 零 `echo-*` 依赖（铁律），manifest 只能用
    // `push_builtin_identity` 声明——它们的 id 常量仍在各自 crate 里。
    //
    // **`echo-sanitize` 不在这一列**（原第 9 个，2026-11 二次收敛）：它的
    // 唯一生产消费者就是 `echo-module-sanitize` 的 `build()`，按
    // `architecture.md` 的判据（消费者 = 1 → 合并）不该独立成 crate；
    // 而脱敏本身也不是「禁用后起不来」的地基，占着禁不掉的 kernel 位等于
    // 给运维一个假动作。实现与身份现同在 `echo-module-sanitize`
    // （`kind = "security"`，唯一关闭路径是 `core.toml`）。
    push_builtin_identity(&mut catalog, version, echo_defs::MODULE_ID)?;
    push_builtin_identity(&mut catalog, version, echo_config::MODULE_ID)?;
    for module in echo_protocol::module::modules(version) {
        push_module(&mut catalog, module)?;
    }
    for module in echo_session::module::modules(version) {
        push_module(&mut catalog, module)?;
    }

    // ── 会话子系统（事件日志 / 投影 / 持久化 / 压缩编排）──
    //
    // 第一个挂：它 provide 的 `echo-agent.session.factory` 是"造一个 trunk"
    // 的唯一入口，排在其它模块之前可以让后续模块在 mount 期就能解析到。
    // 它自己不依赖任何服务（工厂是纯构造），所以位置只影响可读性。
    push_module(&mut catalog, echo_module_session::module(version))?;

    // ── 内置工具集（工具清单所有权在模块里）──
    push_module(
        &mut catalog,
        echo_agent::tool::builtin::builtin_module::module(version),
    )?;

    // ── Shell 会话（`bash-start` / `bash-exec` / `bash-stop` + 命令域）──
    //
    // 2026-11 从工具集拆出：它有自己的机制依赖（`echo-module-shell` 的进程组
    // 生命周期）、自己的面板页面、独立的启停需求。顺序紧跟工具集——不是因为
    // 依赖（它不依赖 tools.builtin），而是让"工具类模块"在面板里相邻。
    push_module(&mut catalog, echo_module_shell::module(version))?;

    // ── 技能目录（整表 + 命令域）──
    push_module(&mut catalog, echo_agent::skill::module(version))?;

    // ── 循环模式（二选一，纯策略）──
    //
    // 驱动本体只有一个（echo-module-loop TurnRunner，2026-10 单轨化），两个模式模块
    // 只表达策略（per-persona 白名单推导的 `LoopMode`）；mount/unmount 不切换
    // 驱动、不注册任何运行期资源。
    //
    // 协议化：模块族声明 `Service{key:"loop"}` 贡献；runner 实例由模块在 mount
    // 里构造并 provide，服务键与贡献声明同源。
    for module in echo_module_loop::modules(version) {
        push_module(&mut catalog, module)?;
    }

    // ── 工作区会话（命令域 + 提示词块）──
    push_module(&mut catalog, echo_module_workspace::module(version))?;

    // ── 联邦（命令域 + 远程工具路由）──
    push_module(&mut catalog, echo_module_federation::module(version))?;

    // ── subagent 委派（纯身份：工具与 hook 在 echo-agent 侧实现）──
    push_builtin_identity(
        &mut catalog,
        version,
        echo_agent::modules::SUBAGENT_MODULE_ID,
    )?;

    // ── 显示层（时间线投影 + 会话视图命令域）──
    //
    // 2026-11 S8 自 management 外置：连接面与显示面分轴。会话视图宿主接缝
    // 的线键不变（`echo-agent.host.session_view`），只是注册方换到了这里。
    push_module(&mut catalog, echo_module_transcript::module(version))?;

    // ── 管理面（Panel 通道 + 会话视图命令域）──
    push_module(&mut catalog, echo_module_management::module(version))?;

    // ── 脱敏（安全不变量；`kind = "security"` → 进程级，人格关不掉）──
    //
    // 纯身份模块：脱敏器在**挂载之前**就由组合根构造（它是 LLM 出口的
    // 装饰器，必须随 `llm` 服务一起进 `Ctx`）——见该 crate 的模块文档。
    // 进目录的意义是：可观测（Panel 清单）、可配置（`modules.toml` 的
    // builtin 行）、进文档门禁、换实现只需换模块。
    for module in echo_module_sanitize::modules(version) {
        push_module(&mut catalog, module)?;
    }

    // ── LLM provider（工厂 + 池规则 + API 命令域）──
    push_module(&mut catalog, echo_module_provider_llm::module(version))?;

    // ── 自更新（updater 单元 + 状态文件）──
    push_module(&mut catalog, echo_module_self_update::module(version))?;

    // 外部仓的进程内模块和本仓目录走同一套身份、配置与冲突检查。
    for module in additional {
        push_module(&mut catalog, module)?;
    }

    reject_duplicate_commands(&catalog)?;
    reject_reserved_commands(&catalog)?;
    Ok(catalog)
}

/// 平台实例模块（id 由 `[instances.<平台>]` 的段名派生）。
///
/// 它是**门控 + 启停壳**：实现由独立进程（`echo-adapter-<platform>`）提供。
/// 参数只剩平台名（配置事实）；启停控制从 [`crate::host_services::INSTANCE_CONTROL`]
/// 服务键解析——那是宿主的运行期句柄，不该出现在目录签名里。
pub(crate) fn instance_module(version: &str, platform: &str) -> Arc<dyn Module> {
    let module_id = crate::instance_manifests::module_id_for(platform);
    let name = format!("{platform} 模块实例");
    let description = format!("{platform} 平台实例模块（独立进程）");
    let module_id_for_mount = module_id.clone();
    Arc::new(BuiltinModule::new(
        ModuleManifest::builtin(
            &module_id,
            &name,
            version,
            "instance",
            platform,
            description,
        )
        // 实例模块化收尾（P1.9，2026-10）：实现由独立进程提供，这里只是
        // 门控壳。必须显式声明，否则面板按 builtin=true 把它显示成
        // 「内部模块」。
        .with_runtime(ModuleRuntime::Subprocess),
        move |mctx| {
            // 启停控制是可选的：没有实例句柄的部署（不跑平台）照样能挂上。
            let control = mctx.service(&crate::host_services::INSTANCE_CONTROL);
            if let Some(control) = &control {
                if control.is_wired() {
                    let control = Arc::clone(control);
                    tokio::spawn(async move { control.start().await });
                }
            }
            let module_id = module_id_for_mount.clone();
            Ok(vec![Disposer::from_fn(move || {
                // 卸载：停实例（门控由宿主的挂载/卸载事件统一拨）。
                if let Some(control) = &control {
                    let control = Arc::clone(control);
                    tokio::spawn(async move { control.stop().await });
                }
                let _ = &module_id;
            })])
        },
    ))
}

/// 外部模块条目（`modules.toml` 的非 builtin 行）的**名单壳**。
///
/// 子进程由 supervisor 拉起，这里的 mount 是**名义挂载**——它让条目出现在面板
/// 的模块清单里（此前用户装的模块在清单里根本看不到）。禁用要重启 Core 生效，
/// 重启按钮对这类条目回「请重启 Core」。
pub(crate) fn external_module(version: &str, entry: &ExternalEntry) -> Arc<dyn Module> {
    Arc::new(BuiltinModule::new(
        // `builtin = false`：来源维度上它不是编译进本二进制的（面板的来源徽标
        // 读这个字段，分组读 `runtime`——两者正交，别互相顶替）。
        ModuleManifest::builtin(
            &entry.id,
            &entry.name,
            version,
            "tool",
            &entry.id,
            format!("外部模块（modules.toml，{}）", entry.kind),
        )
        .with_runtime(ModuleRuntime::Subprocess)
        .with_builtin_flag(false),
        |_ctx| Ok(vec![]),
    ))
}

/// 一个外部模块条目（`modules.toml` 的 `kind != "builtin"` 行）。
pub(crate) struct ExternalEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
}

/// 压入一个**只有身份**的内置模块（tool / hook 实现在宿主侧，见该模块文档）。
fn push_builtin_identity(
    catalog: &mut Vec<Arc<dyn Module>>,
    version: &str,
    id: &str,
) -> anyhow::Result<()> {
    let (name, kind, route, description) = match id {
        echo_agent::modules::SUBAGENT_MODULE_ID => (
            "Subagent 委派",
            "tool",
            "subagent",
            "委派独立子任务给隔离上下文的子 agent：spawn_subagent 工具 + 完成后 <subagent_event> hook 回灌主 agent",
        ),
        // 零 `echo-*` 依赖的两个 crate（铁律）：manifest 只能在这里声明。
        echo_defs::MODULE_ID => (
            "词汇与接缝",
            "kernel",
            "defs",
            "定义层：9 个能力接缝 trait 与全部跨边界词汇（零 echo-* 依赖）",
        ),
        echo_config::MODULE_ID => (
            "配置存储",
            "kernel",
            "config",
            "TOML 原子读改写（tmp+rename，不认识任何段语义）",
        ),
        _ => (id, "tool", id, "内置模块"),
    };
    push_builtin(
        catalog,
        ModuleManifest::builtin(id, name, version, kind, route, description),
        |_ctx| Ok(vec![]),
    )
}

/// 声明了**宿主保留命令**的模块是死代码：命令泵在分发前就把它挡掉了
/// （`command_is_host_reserved`），模块永远收不到，且**没有任何提示**。
///
/// 这正是 `RequestSelfUpdate` / `MigrateSession` 曾经的形状——保留在名单里
/// + 实现只在模块侧 = 必失败。装配期报出来，这类"声明了但收不到"就不可能
/// 再溜进去。
fn reject_reserved_commands(catalog: &[Arc<dyn Module>]) -> anyhow::Result<()> {
    for module in catalog {
        let id = module.manifest().id.clone();
        for command in module.handled_commands() {
            if echo_agent::modules::command_is_host_reserved(&command) {
                return Err(anyhow::anyhow!(
                    "{id} declares host-reserved command {command}: the dispatcher will never \
                     route it to the module (remove the declaration or the reservation)"
                ));
            }
        }
    }
    Ok(())
}

/// 目录形状的**可执行守卫**（全模块化 S6 的回归防线）。
///
/// 模块零捕获之后，"我需要什么"写在模块的 `inject()` 里，"我能给什么"写在
/// 宿主的 [`crate::host_services::register`] 里。两者跨 crate、跨文件，编译器
/// 看不见——对不上的表现是**启动期挂载失败**（`MissingService`）。这组测试
/// 把它提前到 `cargo test`。
#[cfg(test)]
mod catalog_shape_tests {
    use super::*;

    /// 每个模块声明的硬依赖，宿主都得真的会注册。
    ///
    /// 反向不检查（宿主注册了但没人用的键是允许的：那是"将来的模块"的接缝，
    /// 也是诊断面）。
    #[test]
    fn every_declared_dependency_is_registered_by_the_host() {
        let provided = crate::host_services::provided_keys();
        let catalog = catalog("0.0.0-test").expect("目录构造");
        for module in &catalog {
            let id = module.manifest().id.clone();
            for key in module.inject().required {
                assert!(
                    provided.contains(&key),
                    "模块 {id} 声明硬依赖 `{key}`，但宿主没有注册这个键。\n\
                     能注册的键：{provided:?}\n\
                     → 在 `host_services::register` 里加一行，或从模块的 inject 里去掉"
                );
            }
        }
    }

    /// **每个**内置模块的 kind 都必须在 `KIND_SCOPE` 里登记过作用域。
    ///
    /// 这是按 kind 派生门控的唯一风险点：新增一种 kind 而忘了想"它该不该随
    /// persona 白名单关掉"时，`scope_of_kind` 会 fail-closed 到进程级——
    /// 表现为"用户勾了却用不了"，很难查。所以在**装配期**就报错，并点名
    /// 缺哪个 kind。
    ///
    /// 不手抄 kind 表（那又是一份真相）：遍历真实 catalog 的 manifest。
    #[test]
    fn every_catalog_kind_is_classified() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        let mut unclassified = Vec::new();
        for module in &catalog {
            let manifest = module.manifest();
            if !echo_agent::modules::kind_is_classified(&manifest.kind) {
                unclassified.push(format!("{} (kind={})", manifest.id, manifest.kind));
            }
        }
        assert!(
            unclassified.is_empty(),
            "以下模块的 kind 未在 `echo_agent::modules::KIND_SCOPE` 登记，\
             新增 kind 必须显式声明作用域（Persona = 可被 persona 白名单关掉；\
             Process = 进程级）：\n{}",
            unclassified.join("\n")
        );
    }

    /// 按 kind 派生出的 persona 门控集合，**逐项覆盖**改造前那份硬编码清单。
    ///
    /// 这是删掉 `GATED_MODULE_IDS` 的安全绳：先证明"新机制 ≡ 旧行为"，再删。
    /// 清单值照抄历史（不引用已弃用常量——它下一步就没了）。
    #[test]
    fn catalog_gating_covers_the_legacy_entries() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        let registered: Vec<(String, String)> = catalog
            .iter()
            .map(|module| {
                let manifest = module.manifest();
                (manifest.id.clone(), manifest.kind.clone())
            })
            .collect();
        let gated = echo_agent::modules::persona_scoped_ids(&registered);

        // 改造前 `GATED_MODULE_IDS: [&str; 5]` 的全部成员。
        for id in [
            "echo-agent.tools.builtin",
            "echo-agent.shell",
            "echo-agent.skills.dir",
            "echo-agent.workspace",
            "echo-agent.subagent",
        ] {
            assert!(
                gated.iter().any(|g| g == id),
                "旧门控项 {id} 未被按 kind 派生的集合覆盖，实际: {gated:?}"
            );
        }

        // 反向：进程级模块不该出现（它们在旧清单里也从来没有）。
        for id in [
            "echo-agent.provider.llm",
            "echo-agent.loop.single",
            "echo-agent.management.panel",
            "echo-agent.federation",
            "echo-agent.selfupdate",
            "echo-agent.session",
            "echo-agent.transcript",
        ] {
            assert!(
                !gated.iter().any(|g| g == id),
                "进程级模块 {id} 不该进 persona 门控集合，实际: {gated:?}"
            );
        }
    }

    /// `BUILTIN_MODULE_IDS` 与 catalog **双向相等**（谁少了都会点名）。
    ///
    /// 那份常量有三个消费方：更新器校验清单（`update.sh`）、文档门禁
    /// （`modules.md` 必须登记每个内置模块）、以及"这模块是内部还是外部"的
    /// 判断。它本身是手写的——**但手写清单只有在被真实来源校验时才安全**，
    /// 否则漏一个 id 只会让三处校验静默变弱（"更新后二进制缺模块"不再报警）。
    ///
    /// 这里用 catalog（真实来源）做双向比对：新增模块忘了登记 → 红；登记了
    /// 但没进 catalog → 也红。不改成"从 catalog 派生"是因为依赖方向：catalog
    /// 在 core，`BUILTIN_MODULE_IDS` 在 echo-agent，后者看不到前者。
    /// kernel 模块**禁不掉**：`modules.toml` 里 `enabled = false` 必须被
    /// `disabled_kernel_modules` 抓到（启动据此失败），而能力模块照旧可关。
    ///
    /// 这条锁的是「不给假动作」：如果哪天有人把 kernel 从这张表里漏掉，
    /// 运维写 `enabled = false` 就会被静默忽略，而系统半死不活地起来。
    #[test]
    fn kernel_modules_cannot_be_disabled_but_capabilities_can() {
        use echo_agent::modules::{disabled_kernel_modules, MountStep};

        let catalog = catalog("0.0.0-test").expect("目录构造");
        let ids: Vec<String> = catalog
            .iter()
            .map(|module| module.manifest().id.clone())
            .collect();

        // 每个 kernel crate 的 id 都要在 catalog 里（否则下面的门禁不生效）。
        for id in [
            echo_defs::MODULE_ID,
            echo_config::MODULE_ID,
            echo_protocol::module::MODULE_ID,
            echo_session::module::MODULE_ID,
        ] {
            assert!(ids.iter().any(|c| c == id), "kernel {id} 不在 catalog 里");
        }

        // ① 能力模块可关：不报违规。
        let plan = vec![MountStep::new(echo_agent::modules::SHELL_MODULE_ID, false)];
        assert_eq!(disabled_kernel_modules(&ids, &plan), None);

        // ② kernel 模块不可关：必须被抓到并点名。
        let plan = vec![
            MountStep::new(echo_agent::modules::SHELL_MODULE_ID, false),
            MountStep::new(echo_session::module::MODULE_ID, false),
        ];
        assert_eq!(
            disabled_kernel_modules(&ids, &plan).as_deref(),
            Some(echo_session::module::MODULE_ID)
        );

        // ③ 写错 id 的 `enabled = false` 不算违规（它本来就挂不上，
        //    由 mount plan 诊断去 warn，不该因此让启动失败）。
        let plan = vec![MountStep::new("echo-agent.kernel.nonexistent", false)];
        assert_eq!(disabled_kernel_modules(&ids, &plan), None);
    }

    #[test]
    fn builtin_id_list_matches_the_catalog_exactly() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        let from_catalog: std::collections::BTreeSet<String> = catalog
            .iter()
            // catalog 同时装着外部仓的可选进程内模块；它们故意不属于
            // Core 的默认 BUILTIN_MODULE_IDS，不能把“可选”误报成漏登记。
            .filter(|module| module.manifest().id != echo_module_workspace::MODULE_ID)
            .map(|module| module.manifest().id.clone())
            .collect();
        let declared: std::collections::BTreeSet<String> = echo_agent::modules::BUILTIN_MODULE_IDS
            .iter()
            .map(|id| (*id).to_string())
            .collect();

        let missing_from_list: Vec<&String> = from_catalog.difference(&declared).collect();
        let missing_from_catalog: Vec<&String> = declared.difference(&from_catalog).collect();
        assert!(
            missing_from_list.is_empty() && missing_from_catalog.is_empty(),
            "`echo_agent::modules::BUILTIN_MODULE_IDS` 与 catalog 漂移：\n\
             catalog 有、清单缺：{missing_from_list:?}\n\
             清单有、catalog 缺：{missing_from_catalog:?}\n\
             → 补 `source/modules/echo-agent/src/modules.rs` 的常量，并同步 \
             `scripts/update.sh` 的 expected_ids"
        );
    }

    /// `llm` 键的 ownership 不得再漂移（2026-11 S10 的更正）。
    ///
    /// 这条守着一段真实的历史误解：`echo-module-provider-llm` 里曾有个叫
    /// `SERVICE_KEY` 的常量，值是 `"llm"`，文档写"本模块贡献的服务键"——而
    /// 它**并不提供**这个键（提供的是命令域接缝 `PROVIDER_HOST`）。真正的
    /// 注册方是组合根，消费方是 `loop` 模块。于是"谁拥有 `llm`"读代码会读错。
    ///
    /// 现在键住在定义层（`echo_defs::llm::LLM_PROVIDER_SERVICE_KEY`），三方
    /// 都引用它。这条测试锁住三件事：
    /// ① 宿主声明会提供它；② 消费方（loop）确实声明依赖它；
    /// ③ 没有任何内置模块"认领"它是自己 provide 的（名义 ownership 归宿主）。
    #[test]
    fn llm_service_key_has_exactly_one_owner() {
        let key = echo_defs::llm::LLM_PROVIDER_SERVICE_KEY;
        assert!(
            crate::host_services::provided_keys().contains(&key.to_string()),
            "宿主必须声明提供 {key}（组合根按配置构建默认 provider）"
        );

        let catalog = catalog("0.0.0-test").expect("目录构造");
        let loop_depends = catalog.iter().any(|module| {
            module.manifest().kind == "loop" && module.inject().required.iter().any(|k| k == key)
        });
        assert!(loop_depends, "loop 模块必须声明依赖 {key}");

        // ③ 模块侧的**别名**必须指向定义层（不是又写一遍字面量）。
        assert_eq!(
            echo_module_provider_llm::PROVIDER_KEY,
            key,
            "provider 模块的别名必须指向定义层常量，不能自己再写一遍 \"llm\""
        );

        // ④ 源码里不许再出现那两种历史形状（各自独立的一份字面量）。
        //    这是**真的**防回归：行为对、名字错的漂移只有源码能抓到。
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .expect("repo root")
            .join("source");
        let banned = ["SERVICE_KEY: &str = \"llm\"", "Inject::new([\"llm\"])"];
        // 定义层自己就是那份字面量的家；注释里提到旧形状是**说明**不是副本。
        let definition = root.join("modules/echo-defs/src/llm.rs");
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

    /// 孤儿命令：模块声明要接管的命令，若没有任何模块认领，是**静默死路**
    /// （面板点了没反应）。反向由各模块自己的"声明 ⇒ 认领"测试守。
    #[test]
    fn no_command_is_declared_twice_across_the_catalog() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        reject_duplicate_commands(&catalog).expect("命令不得被两个模块同时声明");
    }

    /// 目录里不许有幽灵条目：id 的空段（`echo-agent.instance.`）是派生 bug。
    #[test]
    fn additional_modules_share_identity_validation_and_command_checks() {
        use echo_module::{BuiltinModule, ModuleManifest};
        let extra: Arc<dyn Module> = Arc::new(BuiltinModule::new(
            ModuleManifest::builtin("example.web", "Web", "0", "panel", "web", "external"),
            |_| Ok(Vec::new()),
        ));
        let catalog = catalog_with("0.0.0-test", [extra]).unwrap();
        assert!(catalog.iter().any(|p| p.manifest().id == "example.web"));
    }

    #[test]
    fn catalog_ids_are_well_formed() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        for module in &catalog {
            validate_module_id(&module.manifest().id).expect("id 形状");
        }
    }

    /// **静态目录不含配置驱动项**：平台实例模块与外部的 `modules.toml` 条目
    /// 由调用点追加（它们的"存在"取决于这份部署，不是二进制里有什么）。
    #[test]
    fn static_catalog_has_no_deployment_specific_entries() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        for module in &catalog {
            let id = module.manifest().id.clone();
            assert!(
                !id.starts_with(crate::instance_manifests::MODULE_ID_PREFIX),
                "平台实例模块不该出现在静态目录里：{id}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 派生 id 的空段必须被拦下（真实踩到过：无 `[instances]` 配置时出现
    /// 一个叫 `echo-agent.instance.` 的幽灵模块）。
    #[test]
    fn module_id_shape_is_validated() {
        assert!(validate_module_id("echo-agent.instance.qq").is_ok());
        assert!(validate_module_id("echo-agent.tools.builtin").is_ok());
        assert!(validate_module_id("").is_err());
        assert!(validate_module_id("echo-agent.instance.").is_err());
        // 中间的空段是既有 id 形态（`<prefix>.<name>` 之外没人管），不拦。
        assert!(validate_module_id("echo-agent..x").is_ok());
    }

    /// 同一条命令被两个模块声明 → 装配期失败（否则先挂上的静默赢）。
    #[test]
    fn duplicate_command_declarations_are_rejected() {
        let one = Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("a.one", "a", "0", "tool", "a", "a"),
                |_| Ok(vec![]),
            )
            .with_commands(["Shared"]),
        );
        let two = Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("a.two", "b", "0", "tool", "b", "b"),
                |_| Ok(vec![]),
            )
            .with_commands(["Shared"]),
        );
        let err = reject_duplicate_commands(&[one, two]).expect_err("重名必须报错");
        assert!(err.to_string().contains("Shared"), "{err}");
    }

    /// 会话子系统必须出现在目录里（2026-11 S6 外置后的回归锚点）。
    ///
    /// 它此前是 `echo-agent/src/session/` 下的宿主模块，不在目录里；外置成
    /// 模块后如果漏登记，`Agent::new` 仍能编译（工厂是普通类型），但面板的
    /// 模块清单里就少一个——这条断言把"漏登记"变成启动期失败。
    #[test]
    fn catalog_contains_the_session_module() {
        let catalog = catalog("0.0.0-test").expect("目录构造");
        let ids: Vec<&str> = catalog.iter().map(|p| p.manifest().id.as_str()).collect();
        assert!(
            ids.contains(&echo_module_session::MODULE_ID),
            "会话模块未登记：{ids:?}"
        );
    }

    /// 声明宿主保留命令 → 装配期失败（分发器永远不会路由给它）。
    #[test]
    fn reserved_command_declarations_are_rejected() {
        let module = Arc::new(
            BuiltinModule::new(
                ModuleManifest::builtin("a.bad", "bad", "0", "tool", "bad", "bad"),
                |_| Ok(vec![]),
            )
            .with_commands(["RequestModulesList"]),
        );
        let err = reject_reserved_commands(&[module]).expect_err("保留命令必须报错");
        assert!(err.to_string().contains("RequestModulesList"), "{err}");
    }
}

/// 同一条命令被两个内置模块声明 = **静默遮蔽**（先挂上的那个赢，另一个永远
/// 收不到）——这是注册关系层面的装配 bug，启动期就报出来。
///
/// 为什么要在这里查：命令名与归属都是各模块 crate 自己声明的（这正是 S5 的
/// 成果），框架分发时只是"问一遍谁认领"。跨 crate 的重名没有任何编译器守得住。
fn reject_duplicate_commands(catalog: &[Arc<dyn Module>]) -> anyhow::Result<()> {
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for module in catalog {
        let id = module.manifest().id.clone();
        for command in module.handled_commands() {
            if let Some(previous) = seen.insert(command.clone(), id.clone()) {
                return Err(anyhow::anyhow!(
                    "command {command} is claimed by both {previous} and {id}"
                ));
            }
        }
    }
    Ok(())
}
