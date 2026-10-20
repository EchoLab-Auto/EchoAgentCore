//! 技能命令域：9 条 `*Skill*` 命令由本插件处理。
//!
//! ## 为什么技能命令归本插件
//!
//! `SkillRegistry`（发现 / 分层合并 / 启停）、`loader`（frontmatter）、
//! `install`（Git 来源、`.sources.json`）本来都住在这里。宿主侧的
//! `emit_skills_list` / `save_skill_file` / `delete_skill_file` /
//! `reload_skills` 却各留一份"怎么读、怎么写、怎么重载"的知识——等于同一个
//! 能力被劈成两半。现在命令域整体搬进来，宿主只剩**人格维度**的三件事
//! （见下面的接缝表）。
//!
//! ## 宿主接缝 = 五项能力
//!
//! | 方法 | 为什么在宿主 |
//! |---|---|
//! | [`SkillsCommandHost::registry`] | 技能表是 **per-persona** 的（`Agent` 持有） |
//! | [`SkillsCommandHost::skills_dir`] | 写目录来自 `[agent].skills_dir` 配置 |
//! | [`SkillsCommandHost::reload`] | 重载要跨 `skills_dirs` 并刷新提示词缓存、逐人格生效 |
//! | [`SkillsCommandHost::set_enabled`] | 启停要**持久化**（`disabled_skills`）+ 全局策略 + 逐人格重算 |
//! | [`SkillsCommandHost::emit`] | 面板事件出口（team 标注 / 总线 / 汇聚点） |
//!
//! 这五项的共同点：它们要么需要 `AgentConfig`（宿主的配置类型），要么需要
//! "进程里有哪些人格"（宿主的管理面）。**文件格式、目录校验、列表投影、
//! git 子进程**全在插件里——那些才是能力本体。

use std::sync::Arc;

use echo_protocol::{BackendCommand, BackendEvent, SkillInfo, SkillSourceInfo};
use crate::SkillRegistry;

/// 本插件处理的命令名。
pub const COMMANDS: [&str; 8] = [
    "RequestSkillsList",
    "ReloadSkills",
    "ToggleSkill",
    "SaveSkill",
    "DeleteSkill",
    "InstallSkillFromGit",
    "UpdateSkillFromGit",
    "RemoveSkillSource",
];

/// 宿主接缝：技能域需要而插件不该知道的五件事。
pub trait SkillsCommandHost: Send + Sync {
    /// 本 persona 的技能表。
    fn registry(&self) -> Arc<tokio::sync::Mutex<SkillRegistry>>;
    /// 写目录（`[agent].skills_dir`）。
    fn skills_dir(&self) -> String;
    /// 按写目录重载（跨 `skills_dirs`，逐人格生效，刷新提示词缓存）。
    fn reload(
        &self,
        write_dir: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, String>> + Send + '_>>;
    /// 启停一个技能：改表 + 持久化 + 全局策略 + 逐人格重算。
    ///
    /// 返回 `false` = 技能不存在。异步签名（boxed future，trait 需对象安全）：
    /// 实现里要拿写锁 + 写配置 + 逐人格重算，全是 async。
    fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>>;
    /// 事件出口。
    fn emit(&self, event: BackendEvent);
}

/// 命令处理器。
pub struct SkillsCommandHandler {
    host: Arc<dyn SkillsCommandHost>,
}

impl SkillsCommandHandler {
    pub fn new(host: Arc<dyn SkillsCommandHost>) -> Self {
        Self { host }
    }

    /// 处理一条命令；`None` = 不是本域。
    pub async fn handle(
        &self,
        name: &str,
        payload: serde_json::Value,
    ) -> Option<Result<serde_json::Value, String>> {
        if !COMMANDS.contains(&name) {
            return None;
        }
        let tagged = serde_json::json!({ name: payload });
        let cmd: BackendCommand = match serde_json::from_value(tagged) {
            Ok(cmd) => cmd,
            Err(error) => return Some(Err(format!("skills 命令载荷解析失败（{name}）: {error}"))),
        };
        Some(self.apply(cmd).await.map(|()| serde_json::Value::Null))
    }

    async fn apply(&self, cmd: BackendCommand) -> Result<(), String> {
        match cmd {
            BackendCommand::RequestSkillsList => {
                self.emit_list().await;
                Ok(())
            }
            BackendCommand::ReloadSkills => {
                let dir = self.host.skills_dir();
                match self.host.reload(&dir).await {
                    Ok(any) => {
                        self.emit_list().await;
                        self.notice(if any {
                            "skills reloaded".to_string()
                        } else {
                            "no skills found in the configured directories".to_string()
                        });
                    }
                    Err(error) => self.notice(format!("reload skills failed: {error}")),
                }
                Ok(())
            }
            BackendCommand::ToggleSkill { name, enabled } => {
                let ok = self.host.set_enabled(&name, enabled).await;
                if ok {
                    self.emit_list().await;
                }
                self.notice(if ok {
                    format!(
                        "skill {name} {}",
                        if enabled { "enabled" } else { "disabled" }
                    )
                } else {
                    format!("skill {name} not found")
                });
                Ok(())
            }
            BackendCommand::SaveSkill {
                name,
                description,
                keywords,
                always,
                category,
                content,
                system,
            } => {
                let dir = self.host.skills_dir();
                let draft = SkillDraft {
                    name: name.clone(),
                    description,
                    keywords,
                    always,
                    category,
                    content,
                    system,
                };
                match save_skill_file(&dir, &draft) {
                    Ok(()) => {
                        self.reload_after(dir).await;
                        self.emit_list().await;
                        self.notice(format!("skill {name} saved"));
                    }
                    Err(error) => self.notice(format!("save skill failed: {error}")),
                }
                Ok(())
            }
            BackendCommand::DeleteSkill { name } => {
                let dir = self.host.skills_dir();
                match delete_skill_file(&dir, &name) {
                    Ok(()) => {
                        self.reload_after(dir).await;
                        self.emit_list().await;
                        self.notice(format!("skill {name} deleted"));
                    }
                    Err(error) => self.notice(format!("delete skill failed: {error}")),
                }
                Ok(())
            }
            BackendCommand::InstallSkillFromGit { url, name, branch } => {
                let dir = self.host.skills_dir();
                // git 子进程（clone / fetch）可达秒级至分钟级：整段放 blocking
                // 池，不阻塞 async 调度线程。
                let (dir_c, url_c) = (dir.clone(), url.clone());
                let (name_c, branch_c) = (name.clone(), branch.clone());
                let installed = tokio::task::spawn_blocking(move || {
                    crate::install::install(
                        std::path::Path::new(&dir_c),
                        &url_c,
                        name_c.as_deref(),
                        branch_c.as_deref(),
                    )
                })
                .await
                .unwrap_or_else(|e| Err(format!("install task join failed: {e}")));
                match installed {
                    Ok(_) => {
                        self.reload_after(dir).await;
                        self.emit_list().await;
                        self.notice(format!("skill source installed: {url}"));
                    }
                    Err(error) => self.notice(format!("install skill source failed: {error}")),
                }
                Ok(())
            }
            BackendCommand::UpdateSkillFromGit { name } => {
                let dir = self.host.skills_dir();
                let (dir_c, name_c) = (dir.clone(), name.clone());
                let updated = tokio::task::spawn_blocking(move || {
                    crate::install::update(std::path::Path::new(&dir_c), &name_c)
                })
                .await
                .unwrap_or_else(|e| Err(format!("update task join failed: {e}")));
                match updated {
                    Ok(source) => {
                        self.reload_after(dir).await;
                        self.emit_list().await;
                        self.notice(format!("skill {name} updated to {}", source.rev));
                    }
                    Err(error) => self.notice(format!("update skill source failed: {error}")),
                }
                Ok(())
            }
            BackendCommand::RemoveSkillSource { name } => {
                // `RemoveSkillSource` 只删**来源记录**（不动磁盘上的技能目录）：
                // 用户把某个来源"摘掉徽标"，文件仍在 → 下一轮重载照旧可见。
                let dir = self.host.skills_dir();
                match crate::install::remove_source(std::path::Path::new(&dir), &name) {
                    Ok(()) => self.notice(format!("skill source record removed: {name}")),
                    Err(error) => self.notice(format!("remove skill source failed: {error}")),
                }
                Ok(())
            }
            other => Err(format!(
                "skills 命令域声明了 {} 但处理器没有对应分支",
                other.kind_name()
            )),
        }
    }

    /// 重载并只记日志（保存/删除/安装之后的重载失败不该吞掉用户的操作结果）。
    async fn reload_after(&self, dir: String) {
        if let Err(error) = self.host.reload(&dir).await {
            tracing::warn!(error = %error, "skill reload after mutation failed");
        }
    }

    /// 广播技能清单（`SkillsList`）。
    ///
    /// 来源信息（`.sources.json`）在这里合并：`load_sources_healed` 把旧格式
    /// 记录（键 = 目录名）归正到技能名，否则「目录名 ≠ 技能名」的仓库在
    /// Panel 里看不到来源徽标（2026-10-18 实测：`skill-echo-prodoc/` ↔
    /// `prodoc-format`）。
    async fn emit_list(&self) {
        let dir = self.host.skills_dir();
        let registry = self.host.registry();
        let skills = registry.lock().await;
        let list = skills_list(&skills, std::path::Path::new(&dir));
        drop(skills);
        self.host.emit(BackendEvent::SkillsList { skills: list });
    }

    fn notice(&self, message: impl Into<String>) {
        self.host.emit(BackendEvent::Error {
            session_id: None,
            message: message.into(),
        });
    }
}

/// 技能清单投影（`SkillsList` 的线上形状）——**本 crate 是它的唯一作者**。
///
/// 来源信息（`.sources.json`）在这里合并：`load_sources_healed` 把旧格式记录
/// （键 = 目录名）归正到技能名，否则「目录名 ≠ 技能名」的仓库在 Panel 里看不到
/// 来源徽标（2026-10-18 实测：`skill-echo-prodoc/` ↔ `prodoc-format`）。
///
/// 抽成自由函数的原因与 `instance_list` 相同：宿主还有一处需要它（保存人格后
/// 刷新技能列表），那处不必为此造一个 handler。
pub fn skills_list(skills: &SkillRegistry, skills_dir: &std::path::Path) -> Vec<SkillInfo> {
    let sources = crate::install::load_sources_healed(skills_dir);
    let mut list: Vec<SkillInfo> = skills
        .all()
        .iter()
        .map(|skill| SkillInfo {
            name: skill.metadata.name.clone(),
            description: skill.metadata.description.clone(),
            keywords: skill.metadata.keywords.clone(),
            always: skill.metadata.always,
            enabled: skill.metadata.enabled,
            category: skill.metadata.category.clone(),
            package: skill.metadata.package.clone(),
            content: skill.instructions.clone(),
            system: skill.metadata.system,
            source: sources.get(&skill.metadata.name).map(|src| SkillSourceInfo {
                url: src.url.clone(),
                rev: src.rev.clone(),
                branch: src.branch.clone(),
                installed_at: src.installed_at.clone(),
            }),
        })
        .collect();
    list.sort_by_key(|skill| skill.name.clone());
    list
}

/// 新建/更新技能的载荷（`BackendCommand::SaveSkill` 的字段）。
#[derive(Debug, Clone)]
pub struct SkillDraft {
    pub name: String,
    pub description: String,
    pub keywords: Vec<String>,
    pub always: bool,
    pub category: String,
    pub content: String,
    pub system: bool,
}

/// 技能名 / 目录 key 校验：Unicode 字母数字（覆盖中文）、`-`、`_`、空格；
/// 不允许多级路径、点开头（防路径穿越）。
pub fn validate_skill_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("skill name is empty".into());
    }
    if name.starts_with('.')
        || !name
            .chars()
            .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == ' ')
    {
        return Err(format!(
            "invalid skill name {name:?}: only Unicode letters/digits, '-', '_' allowed"
        ));
    }
    // 路径分隔符显式禁止（`is_alphanumeric` 不含它们，这里兜底明确语义）。
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("skill name must be a single directory key".into());
    }
    Ok(())
}

/// 写 `{skills_dir}/{name}/SKILL.md`（frontmatter + 正文）。
pub fn save_skill_file(skills_dir: &str, draft: &SkillDraft) -> Result<(), String> {
    validate_skill_name(&draft.name)?;
    let dir = std::path::Path::new(skills_dir);
    let skill_dir = dir.join(&draft.name);
    std::fs::create_dir_all(&skill_dir)
        .map_err(|e| format!("cannot create {}: {e}", skill_dir.display()))?;
    let keywords = if draft.keywords.is_empty() {
        String::new()
    } else {
        format!("keywords: [{}]\n", draft.keywords.join(", "))
    };
    let mut frontmatter = format!(
        "---\nname: {}\ndescription: {}\n{keywords}",
        draft.name, draft.description,
    );
    if draft.always {
        frontmatter.push_str("metadata:\n  always: true\n");
    }
    if !draft.category.is_empty() {
        frontmatter.push_str(&format!("category: {}\n", draft.category));
    }
    if draft.system {
        frontmatter.push_str("system: true\n");
    }
    frontmatter.push_str("---\n");
    std::fs::write(skill_dir.join("SKILL.md"), format!("{frontmatter}{}", draft.content))
        .map_err(|e| format!("cannot write SKILL.md: {e}"))?;
    Ok(())
}

/// 删 `{skills_dir}/{name}`（仅在它确实是配置目录内的技能目录时）。
pub fn delete_skill_file(skills_dir: &str, name: &str) -> Result<(), String> {
    validate_skill_name(name)?;
    let root = std::path::Path::new(skills_dir);
    let canonical_root = root
        .canonicalize()
        .map_err(|e| format!("skills dir missing: {e}"))?;
    let skill_dir = root.join(name);
    let canonical_target = skill_dir
        .canonicalize()
        .map_err(|e| format!("skill {name} not found: {e}"))?;
    if !canonical_target.starts_with(&canonical_root) {
        return Err("refusing to delete outside skills directory".into());
    }
    if !canonical_target.join("SKILL.md").exists() {
        return Err(format!("{name} is not a skill directory"));
    }
    std::fs::remove_dir_all(&canonical_target).map_err(|e| format!("cannot delete {name}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct FakeHost {
        registry: Arc<tokio::sync::Mutex<SkillRegistry>>,
        dir: String,
        events: Mutex<Vec<BackendEvent>>,
        enabled_calls: Mutex<Vec<(String, bool)>>,
    }

    impl FakeHost {
        fn new(dir: &std::path::Path) -> Self {
            Self {
                registry: Arc::new(tokio::sync::Mutex::new(SkillRegistry::new())),
                dir: dir.display().to_string(),
                events: Mutex::new(Vec::new()),
                enabled_calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl SkillsCommandHost for FakeHost {
        fn registry(&self) -> Arc<tokio::sync::Mutex<SkillRegistry>> {
            Arc::clone(&self.registry)
        }
        fn skills_dir(&self) -> String {
            self.dir.clone()
        }
        fn reload(
            &self,
            _write_dir: &str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<bool, String>> + Send + '_>,
        > {
            Box::pin(async { Ok(true) })
        }
        fn set_enabled(
            &self,
            name: &str,
            enabled: bool,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + '_>> {
            self.enabled_calls
                .lock()
                .unwrap()
                .push((name.to_string(), enabled));
            let found = name != "ghost";
            Box::pin(async move { found })
        }
        fn emit(&self, event: BackendEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn setup() -> (SkillsCommandHandler, Arc<FakeHost>, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "echo-skills-cmd-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let host = Arc::new(FakeHost::new(&dir));
        (SkillsCommandHandler::new(host.clone()), host, dir)
    }

    async fn call(h: &SkillsCommandHandler, name: &str, payload: serde_json::Value) {
        h.handle(name, payload)
            .await
            .unwrap_or_else(|| panic!("{name} 必须被本域认领"))
            .unwrap_or_else(|e| panic!("{name} 不该解析失败: {e}"));
    }

    /// 保存 → 文件真的落下（frontmatter 与正文都在），并广播清单 + 回执。
    #[tokio::test]
    async fn save_writes_the_skill_file_and_broadcasts() {
        let (h, host, dir) = setup();
        call(
            &h,
            "SaveSkill",
            serde_json::json!({
                "name": "alpha", "description": "第一个", "keywords": ["a","b"],
                "always": false, "category": "测试", "content": "# 正文", "system": false
            }),
        )
        .await;
        let body = std::fs::read_to_string(dir.join("alpha/SKILL.md")).expect("SKILL.md");
        assert!(body.contains("name: alpha"), "{body}");
        assert!(body.contains("keywords: [a, b]"), "{body}");
        assert!(body.contains("# 正文"), "{body}");
        let events = host.events.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, BackendEvent::SkillsList { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            BackendEvent::Error { message, .. } if message.contains("saved")
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 删除：真删目录；不存在的技能报明确错误（不静默成功）。
    #[tokio::test]
    async fn delete_removes_the_directory_and_reports_missing() {
        let (h, host, dir) = setup();
        call(
            &h,
            "SaveSkill",
            serde_json::json!({
                "name": "beta", "description": "", "keywords": [],
                "always": false, "category": "", "content": "x", "system": false
            }),
        )
        .await;
        assert!(dir.join("beta/SKILL.md").exists());
        call(&h, "DeleteSkill", serde_json::json!({"name": "beta"})).await;
        assert!(!dir.join("beta").exists(), "目录必须真的消失");
        call(&h, "DeleteSkill", serde_json::json!({"name": "beta"})).await;
        let events = host.events.lock().unwrap();
        let last = events
            .iter()
            .rev()
            .find_map(|e| match e {
                BackendEvent::Error { message, .. } => Some(message.clone()),
                _ => None,
            })
            .expect("有回执");
        assert!(last.contains("delete skill failed"), "{last}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 名字校验：路径穿越 / 点开头 / 分隔符一律拒绝（安全边界）。
    #[test]
    fn skill_name_validation_rejects_traversal() {
        for bad in ["", ".hidden", "../escape", "a/b", "a\\b", "a..b"] {
            assert!(validate_skill_name(bad).is_err(), "{bad} 应被拒");
        }
        for good in ["alpha", "技能一", "a-b_c"] {
            assert!(validate_skill_name(good).is_ok(), "{good} 应通过");
        }
    }

    /// 启停：委托宿主（持久化 + 全局策略在那边），插件只广播与回执。
    #[tokio::test]
    async fn toggle_delegates_to_the_host_and_reports_not_found() {
        let (h, host, dir) = setup();
        call(&h, "ToggleSkill", serde_json::json!({"name": "alpha", "enabled": false})).await;
        call(&h, "ToggleSkill", serde_json::json!({"name": "ghost", "enabled": true})).await;
        assert_eq!(
            *host.enabled_calls.lock().unwrap(),
            vec![("alpha".to_string(), false), ("ghost".to_string(), true)]
        );
        let events = host.events.lock().unwrap();
        let notices: Vec<&String> = events
            .iter()
            .filter_map(|e| match e {
                BackendEvent::Error { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert!(notices.iter().any(|m| m.contains("alpha disabled")), "{notices:?}");
        assert!(notices.iter().any(|m| m.contains("ghost not found")), "{notices:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 不属于本域的命令返回 `None`（让下一个插件/宿主接手）。
    #[tokio::test]
    async fn foreign_commands_are_not_claimed() {
        let (h, _host, dir) = setup();
        assert!(h
            .handle("RequestToolsList", serde_json::json!({}))
            .await
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `RemoveSkillSource` 只删**来源记录**，不动磁盘上的技能目录。
    #[tokio::test]
    async fn remove_source_only_drops_the_record() {
        let (h, host, dir) = setup();
        call(
            &h,
            "SaveSkill",
            serde_json::json!({
                "name": "gamma", "description": "", "keywords": [],
                "always": false, "category": "", "content": "x", "system": false
            }),
        )
        .await;
        call(&h, "RemoveSkillSource", serde_json::json!({"name": "gamma"})).await;
        assert!(dir.join("gamma/SKILL.md").exists(), "文件必须留存");
        let events = host.events.lock().unwrap();
        assert!(events.iter().rev().any(|e| matches!(
            e,
            BackendEvent::Error { message, .. } if message.contains("source record removed") || message.contains("remove skill source failed")
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
