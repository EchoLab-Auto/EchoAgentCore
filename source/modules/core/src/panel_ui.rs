//! 面板 UI 贡献注册表：模块登记前端 bundle，面板运行期加载。
//!
//! 契约在 [`echo_defs::panel_ui`]（trait + 结构 + 键），本文件是它在 Core 侧
//! 的唯一实现。与 [`crate::panel_services`] 同构（同名覆盖 + 代次 Disposer），
//! 差别只在**载荷是"一个可加载的 URL + 版本"**而不是"一个可调用的 trait 对象"。
//!
//! ## 谁消费它
//!
//! 面板通过面板服务 `echo-agent.panel-ui.list` 拿清单（见
//! [`crate::panel_ui::list_payload`] 与 Core 侧服务实现），然后逐条
//! `import()`。Core 自己不加载任何 JS——它只保管"有哪些"。
//!
//! ## 为什么不做成面板服务的一部分
//!
//! 面板服务的 trait 对象活在 Core 进程里、能被直接调用；UI 贡献只是一份描述。
//! 把它们混在一起会让"列出 UI 模块"这件事必须先有一个模块实现服务，而 UI
//! 贡献本来就该由任意模块在 mount 时直接登记。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use echo_defs::panel_ui::{PanelUiContribution, PanelUiRegistrar};
use echo_defs::Disposer;

/// 一个已登记贡献的槽位。
struct Entry {
    /// 登记代次：同名覆盖时递增，`Disposer` 只撤销自己那一代。
    generation: u64,
    contribution: PanelUiContribution,
}

/// 注册表本体。
#[derive(Default)]
pub(crate) struct PanelUiTable {
    entries: RwLock<HashMap<String, Entry>>,
    next_generation: AtomicU64,
}

impl std::fmt::Debug for PanelUiTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanelUiTable")
            .field("ids", &self.ids())
            .finish()
    }
}

impl PanelUiTable {
    /// 空表。
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// 造一个可放进 `Ctx` 的登记句柄（与本体共享同一张表）。
    pub(crate) fn registrar(self: &Arc<Self>) -> Arc<dyn PanelUiRegistrar> {
        Arc::new(TableHandle {
            table: Arc::clone(self),
        })
    }

    /// 当前全部贡献（按 id 排序，供面板服务 `list` 与诊断用）。
    pub(crate) fn contributions(&self) -> Vec<PanelUiContribution> {
        let entries = self
            .entries
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut rows: Vec<PanelUiContribution> = entries
            .values()
            .map(|entry| entry.contribution.clone())
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows
    }

    /// 当前全部 id（排序；错误消息与诊断用）。
    pub(crate) fn ids(&self) -> Vec<String> {
        self.contributions()
            .into_iter()
            .map(|contribution| contribution.id)
            .collect()
    }

    /// 只在槽位仍是 `generation` 这一代时删除（旧 disposer 不能顶掉新的）。
    fn remove_if_generation(&self, id: &str, generation: u64) {
        let mut entries = self
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entries.get(id).map(|entry| entry.generation) != Some(generation) {
            return;
        }
        entries.remove(id);
        drop(entries);
        tracing::debug!(extension = id, "面板 UI 贡献已撤销");
    }
}

/// 放进 `Ctx` 的登记句柄。
struct TableHandle {
    table: Arc<PanelUiTable>,
}

/// 校验一份贡献；返回可读原因（这些最终会显示在面板的诊断页里）。
///
/// 抽成自由函数是为了让"注册时校验"与"测试"用同一份规则——两份校验必然分叉，
/// 而分叉的后果是面板加载一个 Core 认为合法的坏 URL。
pub(crate) fn validate(contribution: &PanelUiContribution) -> Result<(), String> {
    let id = contribution.id.trim();
    if id.is_empty() {
        return Err("面板 UI 贡献的 id 不能为空".to_string());
    }
    if id.len() > 128 {
        return Err(format!("面板 UI 贡献的 id 过长（{} 字节）", id.len()));
    }
    if id.chars().any(|c| c.is_whitespace() || c == '/') {
        return Err(format!("面板 UI 贡献的 id 不能含空白或 `/`：{id}"));
    }
    let bundle = contribution.bundle.trim();
    if bundle.is_empty() {
        return Err(format!("面板 UI 贡献 {id} 的 bundle 不能为空"));
    }
    // 只接受站点相对路径。绝对 URL / 协议相对 URL 会让面板变成"任意脚本
    // 加载器"——一个被投毒的模块描述就足以在用户浏览器里执行外部代码。
    if bundle.contains("://") || bundle.starts_with("//") {
        return Err(format!(
            "面板 UI 贡献 {id} 的 bundle 必须是站点相对路径（不含协议）：{bundle}"
        ));
    }
    if bundle.starts_with('/') {
        // 前导 `/` 会绕过部署基址（子路径部署下指向站点根），统一要求相对。
        return Err(format!(
            "面板 UI 贡献 {id} 的 bundle 不能以 `/` 开头（应相对于站点基址）：{bundle}"
        ));
    }
    if bundle.split('/').any(|segment| segment == "..") {
        return Err(format!("面板 UI 贡献 {id} 的 bundle 不能含 `..`：{bundle}"));
    }
    if contribution.sdk_version.trim().is_empty() {
        return Err(format!("面板 UI 贡献 {id} 缺 sdk_version"));
    }
    Ok(())
}

impl PanelUiRegistrar for TableHandle {
    fn register(&self, contribution: PanelUiContribution) -> Result<Disposer, String> {
        validate(&contribution)?;
        let id = contribution.id.clone();
        let generation = self.table.next_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let previous = self
            .table
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                id.clone(),
                Entry {
                    generation,
                    contribution: contribution.clone(),
                },
            );
        if previous.is_some() {
            tracing::debug!(
                extension = %id,
                bundle = %contribution.bundle,
                "面板 UI 贡献被覆盖登记（同名贡献的新一代）"
            );
        } else {
            tracing::debug!(
                extension = %id,
                bundle = %contribution.bundle,
                sdk_version = %contribution.sdk_version,
                "面板 UI 贡献已登记"
            );
        }
        let table = Arc::clone(&self.table);
        let owned = id;
        Ok(Disposer::from_fn(move || {
            table.remove_if_generation(&owned, generation);
        }))
    }

    fn unregister(&self, id: &str) {
        let removed = self
            .table
            .entries
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(id);
        if removed.is_some() {
            tracing::debug!(extension = id, "面板 UI 贡献已撤销");
        }
    }
}

/// **外部模块**（进程外）的 UI 贡献：从 `Contribution::Custom` 里收集并登记。
///
/// ## 为什么走 `Custom` 而不是给注册表加一个模块可直接调用的 trait
///
/// 进程外模块拿不到 `Ctx`，只能经协议 `Register` 上报贡献。框架的
/// `Contribution::Custom { custom_kind, payload }` 就是"宿主自定义贡献"的唯一
/// 入口（框架只搬运、不解释），宿主在这里解释 `custom_kind = "panel_ui"`。
///
/// 用 `Custom` 而不是给框架加变体的理由与框架设计一致：面板 UI 是**本宿主**
/// 的领域概念，框架不该认识它。加一个变体意味着所有框架使用者都要跟着改。
///
/// ## 线上形状
///
/// ```json
/// { "custom_kind": "panel_ui",
///   "payload": { "name": "工作区会话",
///                "bundle": "panel-extensions/echo-agent.workspace/index.js",
///                "sdk_version": "1" } }
/// ```
///
/// `id` 取自贡献的 `name` 字段（`Contribution::Custom.name` 参与 `Invoke`
/// 寻址，正好承载扩展 id——两者本来就是同一个字符串）。
///
/// 返回撤销句柄；模块卸载时由调用方（组合根）drop。
pub(crate) fn register_external_contributions(
    table: &Arc<PanelUiTable>,
    registrar: &Arc<dyn PanelUiRegistrar>,
    handles: &[echo_module_host::ModuleHandle],
) -> Vec<Disposer> {
    let mut keeps = Vec::new();
    for handle in handles {
        for contribution in handle.contributions() {
            let echo_module_host::api::Contribution::Custom {
                kind,
                name,
                payload,
            } = contribution
            else {
                continue;
            };
            if kind != PANEL_UI_CONTRIBUTION_KIND {
                continue;
            }
            let parsed = PanelUiContribution {
                id: name.clone(),
                name: payload
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or(name.as_str())
                    .to_string(),
                bundle: payload
                    .get("bundle")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                sdk_version: payload
                    .get("sdk_version")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
            };
            match registrar.register(parsed.clone()) {
                Ok(disposer) => {
                    tracing::debug!(
                        extension = %parsed.id,
                        bundle = %parsed.bundle,
                        "外部模块的面板 UI 贡献已登记"
                    );
                    keeps.push(disposer);
                }
                Err(error) => {
                    // 外部模块的声明由它自己写，宿主只校验：坏声明不进表，
                    // 面板上少一块入口，但不影响模块本身工作。
                    tracing::warn!(
                        module = handle.module_id(),
                        extension = %parsed.id,
                        %error,
                        "外部模块的面板 UI 贡献登记失败"
                    );
                }
            }
        }
    }
    let _ = table;
    keeps
}

/// `Contribution::Custom` 里本宿主认的分类名。
pub(crate) const PANEL_UI_CONTRIBUTION_KIND: &str = "panel_ui";

/// 面板服务 `echo-agent.panel-ui`：把当前贡献清单交给面板。
///
/// ## 为什么由 Core 提供而不是某个模块
///
/// 清单是**注册表的投影**，不属于任何一个模块。做在模块里会让"列出 UI 模块"
/// 依赖那个模块是否挂载——一个模块没装，面板就看不到别人也有 UI。
///
/// ## 方法形状
///
/// `list` 的载荷忽略（预留按 id 过滤），回执 `data`：
///
/// ```json
/// { "sdk_version": "1", "extensions": [ { "id", "name", "bundle", "sdk_version" } ] }
/// ```
///
/// 顶层的 `sdk_version` 是**面板自己的**契约版本：面板拿它与自己的常量比对，
/// 不一致说明"这份 Core 的 SDK 契约与面板不匹配"，应当直接报错而不是逐条
/// 加载再失败 N 次。
pub(crate) struct CorePanelUiService {
    table: Arc<PanelUiTable>,
}

impl CorePanelUiService {
    pub(crate) fn new(table: Arc<PanelUiTable>) -> Self {
        Self { table }
    }
}

/// 服务名（面板硬编码它，跨仓契约）。
pub(crate) const SERVICE_NAME: &str = "echo-agent.panel-ui";

#[async_trait::async_trait]
impl echo_defs::panel_service::PanelService for CorePanelUiService {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    fn methods(&self) -> &'static [&'static str] {
        &["list"]
    }

    async fn invoke(
        &self,
        method: &str,
        _payload: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if method != "list" {
            // 注册表已按 methods() 过滤过一次；这里是第二道保险。
            return Err(format!("面板 UI 服务未声明方法 {method}"));
        }
        let extensions: Vec<serde_json::Value> = self
            .table
            .contributions()
            .into_iter()
            .map(|contribution| {
                serde_json::json!({
                    "id": contribution.id,
                    "name": contribution.name,
                    "bundle": contribution.bundle,
                    "sdk_version": contribution.sdk_version,
                })
            })
            .collect();
        Ok(serde_json::json!({
            "sdk_version": echo_defs::panel_ui::PANEL_UI_SDK_VERSION,
            "extensions": extensions,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use echo_defs::panel_ui::PANEL_UI_SDK_VERSION;

    fn contribution(id: &str) -> PanelUiContribution {
        PanelUiContribution {
            id: id.to_string(),
            name: "测试".into(),
            bundle: format!("panel-extensions/{id}/index.js"),
            sdk_version: PANEL_UI_SDK_VERSION.into(),
        }
    }

    #[test]
    fn registered_contribution_is_listed() {
        let table = PanelUiTable::new();
        let _keep = table
            .registrar()
            .register(contribution("echo-agent.workspace"))
            .unwrap();
        let rows = table.contributions();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "echo-agent.workspace");
        assert_eq!(table.ids(), vec!["echo-agent.workspace"]);
    }

    /// 撤销是幂等的：卸载路径可以无脑调。
    #[test]
    fn unregister_is_idempotent() {
        let table = PanelUiTable::new();
        let handle = table.registrar();
        let keep = handle.register(contribution("x")).unwrap();
        handle.unregister("x");
        handle.unregister("x");
        assert!(table.ids().is_empty());
        drop(keep);
        assert!(table.ids().is_empty());
    }

    /// 同名覆盖：旧代次的 disposer 晚到析构**不能**删掉新登记的那份。
    #[test]
    fn stale_disposer_does_not_remove_the_new_generation() {
        let table = PanelUiTable::new();
        let handle = table.registrar();
        let old = handle.register(contribution("x")).unwrap();
        let mut newer = contribution("x");
        newer.bundle = "panel-extensions/x/v2.js".into();
        let new = handle.register(newer).unwrap();
        drop(old);
        let rows = table.contributions();
        assert_eq!(rows.len(), 1, "旧代次析构不应删除新登记");
        assert_eq!(rows[0].bundle, "panel-extensions/x/v2.js");
        drop(new);
        assert!(table.ids().is_empty());
    }

    /// 多个 id 按字典序列出（面板侧无需再排序；也让诊断输出稳定）。
    #[test]
    fn contributions_are_sorted_by_id() {
        let table = PanelUiTable::new();
        let handle = table.registrar();
        let _a = handle.register(contribution("zeta")).unwrap();
        let _b = handle.register(contribution("alpha")).unwrap();
        assert_eq!(table.ids(), vec!["alpha", "zeta"]);
    }

    /// 校验规则：每一条都是"面板会加载坏东西"的具体形态。
    #[test]
    fn validation_rejects_unsafe_bundles() {
        // 空 id / 含 `/` 的 id
        assert!(validate(&contribution("")).is_err());
        assert!(validate(&contribution("a/b")).is_err());
        assert!(validate(&contribution("a b")).is_err());
        // 空 bundle
        let mut c = contribution("x");
        c.bundle = String::new();
        assert!(validate(&c).is_err());
        // 绝对 URL：会让面板变成任意脚本加载器
        c.bundle = "https://evil.example/x.js".into();
        assert!(validate(&c).is_err());
        // 协议相对 URL：同上
        c.bundle = "//evil.example/x.js".into();
        assert!(validate(&c).is_err());
        // 前导 `/`：绕过部署基址
        c.bundle = "/panel-extensions/x/index.js".into();
        assert!(validate(&c).is_err());
        // 路径穿越
        c.bundle = "panel-extensions/../../secret.js".into();
        assert!(validate(&c).is_err());
        // 缺版本
        c.bundle = "panel-extensions/x/index.js".into();
        c.sdk_version = String::new();
        assert!(validate(&c).is_err());
        // 合法形态
        assert!(validate(&contribution("x")).is_ok());
    }

    /// 非法贡献必须在**登记时**就失败，而不是等面板加载到一半。
    #[test]
    fn register_rejects_invalid_contribution() {
        let table = PanelUiTable::new();
        let mut bad = contribution("x");
        bad.bundle = "https://evil.example/x.js".into();
        assert!(table.registrar().register(bad).is_err());
        assert!(table.ids().is_empty(), "非法贡献不该进表");
    }
}
