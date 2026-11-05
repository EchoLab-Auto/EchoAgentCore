//! 外部 Git 来源 skill 的安装/更新管理。
//!
//! - 安装：`git clone` 到 `skills_dir/<name>`（SKILL.md 热重载自动发现）；
//!   clone 后若仓库根没有 SKILL.md，尝试查找子目录中的 SKILL.md 并提升
//!   为安装根（把子目录内容拷到安装目录）。
//! - 来源记录：`skills_dir/.sources.json`（{ name: { url, rev, branch, installed_at, dir } }），
//!   供更新（fetch + reset 到记录 rev/branch）与前端来源徽标展示。
//!   **键是技能名（frontmatter `name`），不是目录名**——`emit_skills_list`
//!   按 `skill.metadata.name` 查，用目录名做键会让「更新 / 移除来源」入口
//!   在「目录名 ≠ 技能名」的仓库上整体失踪（2026-10-18 实测：
//!   `skill-echo-prodoc/` 里的技能叫 `prodoc-format`）。
//! - 更新：git fetch + checkout/reset；失败不动来源记录。
//! - 移除：仅清理来源记录（目录待用户 DeleteSkill）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSource {
    pub url: String,
    #[serde(default)]
    pub rev: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub installed_at: Option<String>,
    /// 实际安装目录名（相对 `skills_dir`）。
    ///
    /// 记录键是**技能名**（`SKILL.md` frontmatter 的 `name`），而目录名可能
    /// 不同——仓库 `skill-echo-prodoc` 里的技能叫 `prodoc-format`（2026-10-18
    /// 实测）。旧记录没有该字段，回退「键即目录名」。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

pub type SkillSources = BTreeMap<String, SkillSource>;

pub fn sources_path(skills_dir: &Path) -> PathBuf {
    skills_dir.join(".sources.json")
}

pub fn load_sources(skills_dir: &Path) -> SkillSources {
    std::fs::read(sources_path(skills_dir))
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .unwrap_or_default()
}

pub fn save_sources(skills_dir: &Path, sources: &SkillSources) -> Result<(), String> {
    let path = sources_path(skills_dir);
    let raw = serde_json::to_vec_pretty(sources).map_err(|e| format!("serialize sources: {e}"))?;
    std::fs::write(&path, raw).map_err(|e| format!("write sources: {e}"))
}

/// 把旧格式的来源记录在**内存里**归正（不写盘，热路径安全）。
///
/// 旧记录键是目录名、且没有 `dir` 字段。若目录里的 `SKILL.md` 声明的
/// `name` 与键不同（`skill-echo-prodoc/` → `prodoc-format`），按旧键永远
/// 查不到来源——Panel 里「更新 / 移除来源」整体不可见。
///
/// 归正规则：读 `<key>/SKILL.md` 的 frontmatter `name`；不同则把记录改挂到
/// 该名下并补 `dir = key`。读不到（目录丢了、frontmatter 坏了）就只补
/// `dir`，不动键。
///
/// 写盘留给 install / update / remove（那时保存的是已归正的全量表），
/// 列表刷新（`emit_skills_list`，每次打开设置页都跑）不做 IO 副作用。
fn heal_sources(skills_dir: &Path, sources: &mut SkillSources) {
    let legacy: Vec<String> = sources
        .iter()
        .filter(|(_, src)| src.dir.is_none())
        .map(|(key, _)| key.clone())
        .collect();
    for key in legacy {
        let skill_name = crate::loader::load_skill(&skills_dir.join(&key).join("SKILL.md"))
            .ok()
            .map(|skill| skill.metadata.name.trim().to_string())
            .filter(|name| !name.is_empty());
        match skill_name {
            Some(name) if name != key => {
                if let Some(mut src) = sources.remove(&key) {
                    src.dir = Some(key.clone());
                    sources.insert(name, src);
                }
            }
            // 键即技能名（或读不到）：只补 dir，保持键不变。
            _ => {
                if let Some(src) = sources.get_mut(&key) {
                    src.dir = Some(key.clone());
                }
            }
        }
    }
}

/// 读取来源记录并归正旧格式（见 [`heal_sources`]）。
pub fn load_sources_healed(skills_dir: &Path) -> SkillSources {
    let mut sources = load_sources(skills_dir);
    heal_sources(skills_dir, &mut sources);
    sources
}

/// 解析 git 仓库名（url 末尾去 .git）
fn repo_name(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let base = trimmed
        .rsplit('/')
        .next()
        .unwrap_or("skill")
        .trim_end_matches(".git");
    if base.is_empty() {
        "skill".to_string()
    } else {
        base.to_string()
    }
}

/// 半成品 clone 判定：有 `.git` 但没有 `SKILL.md`。
///
/// `git clone` 会先创建目标目录再连网络；被硬超时 kill 后连 `.git` 都留下
/// （实测 2026-10-18：github.com:443 不可达 → 120s 超时 → `skills/<name>/.git`
/// 残留）。正常技能目录必有 `SKILL.md`，所以这个组合只可能是失败残留。
fn is_broken_clone(dir: &Path) -> bool {
    dir.join(".git").exists() && !dir.join("SKILL.md").exists()
}

/// 在 git 仓库目录中定位 SKILL.md 根：根目录有则用根；否则第一个
/// 含 SKILL.md 的**直接子目录**（仅扫描一层 read_dir）。
fn find_skill_root(repo: &Path) -> Option<PathBuf> {
    if repo.join("SKILL.md").exists() {
        return Some(repo.to_path_buf());
    }
    let mut found: Option<PathBuf> = None;
    let mut best_depth = usize::MAX;
    if let Ok(entries) = std::fs::read_dir(repo) {
        for entry in entries.flatten() {
            let p = entry.path();
            if !p.is_dir() {
                continue;
            }
            let depth = p.components().count();
            if depth < best_depth && p.join("SKILL.md").exists() {
                best_depth = depth;
                found = Some(p);
            }
        }
    }
    found
}

/// git 子进程硬超时（网络挂起/clone 大仓库的护栏）。
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// 慢速中止：下载速率低于 1KB/s 持续 30s 时中止（git 内建护栏，覆盖
/// 建连后停滞的场景；硬超时兜底其余情形）。
const GIT_LOW_SPEED_LIMIT: &str = "1000";
const GIT_LOW_SPEED_TIME: &str = "30";

/// 运行 git 并捕获输出；带硬超时（超时 kill）与慢速中止环境变量。
///
/// 输出重定向到临时文件而非管道：`try_wait` 轮询期间无人读管，若 git
/// 输出超过内核 pipe 缓冲会自锁（写满 → 子进程阻塞 → 轮询永不结束）。
fn sh(args: &[&str]) -> Result<String, String> {
    fn run(args: &[&str]) -> Result<(std::process::ExitStatus, String, String), String> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let out_path = std::env::temp_dir().join(format!("echo-git-out-{id}"));
        let err_path = std::env::temp_dir().join(format!("echo-git-err-{id}"));
        let out_file =
            std::fs::File::create(&out_path).map_err(|e| format!("git 输出临时文件失败: {e}"))?;
        let err_file =
            std::fs::File::create(&err_path).map_err(|e| format!("git 输出临时文件失败: {e}"))?;
        let mut child = std::process::Command::new("git")
            .args(args)
            .env("GIT_HTTP_LOW_SPEED_LIMIT", GIT_LOW_SPEED_LIMIT)
            .env("GIT_HTTP_LOW_SPEED_TIME", GIT_LOW_SPEED_TIME)
            // 私有仓库 / 凭据缺失时 git 会尝试交互式询问用户名密码，
            // 在无 TTY 的后台进程里直接挂到硬超时（120s）才失败——用户
            // 只看到「卡住两分钟」。禁止提示后立即报认证失败（2026-10-18）。
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(std::process::Stdio::null())
            .stdout(out_file)
            .stderr(err_file)
            .spawn()
            .map_err(|e| format!("git 执行失败: {e}"))?;
        let start = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() >= GIT_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_file(&out_path);
                    let _ = std::fs::remove_file(&err_path);
                    return Err(format!(
                        "git {} 超时（{}s，已中止）",
                        args.first().copied().unwrap_or(""),
                        GIT_TIMEOUT.as_secs()
                    ));
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(150)),
                Err(e) => {
                    let _ = std::fs::remove_file(&out_path);
                    let _ = std::fs::remove_file(&err_path);
                    return Err(format!("git 等待失败: {e}"));
                }
            }
        };
        let stdout = std::fs::read_to_string(&out_path).unwrap_or_default();
        let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
        let _ = std::fs::remove_file(&out_path);
        let _ = std::fs::remove_file(&err_path);
        Ok((status, stdout, stderr))
    }
    let (status, stdout, stderr) = run(args)?;
    if !status.success() {
        return Err(format!("git {} 失败: {}", args[0], stderr.trim()));
    }
    Ok(stdout.trim().to_string())
}

/// 安装：`git clone [--branch X] url skills_dir/name`。
/// clone 后无根 SKILL.md 时把子目录内容提升到安装根（拷贝 + 清空原层）。
pub fn install(
    skills_dir: &Path,
    url: &str,
    name: Option<&str>,
    branch: Option<&str>,
) -> Result<SkillSource, String> {
    if url.trim().is_empty() {
        return Err("url 不能为空".into());
    }
    let dir_name = name
        .filter(|n| !n.trim().is_empty())
        .map(|n| n.to_string())
        .unwrap_or_else(|| repo_name(url));
    if dir_name.starts_with('.') || dir_name.contains('/') || dir_name.contains('\\') {
        return Err(format!("无效的安装目录名: {dir_name:?}"));
    }
    let target = skills_dir.join(&dir_name);
    if target.exists() {
        // 上次失败（clone 被超时 kill）留下只有 `.git` 的半成品：自动清掉，
        // 否则用户会**永远**撞「目录已存在」而不知为何——2026-10-18 实况：
        // 一次 HTTPS 超时之后，同一个名字再也装不上，且错误文案只让「换名」。
        if is_broken_clone(&target) {
            std::fs::remove_dir_all(&target)
                .map_err(|e| format!("清理上次失败的残留目录失败: {dir_name}: {e}"))?;
        } else {
            return Err(format!(
                "目录已存在: {dir_name}（若是之前安装的，请先卸载或换名安装；                 若确认是手动放置的同名目录，请先移走）"
            ));
        }
    }
    std::fs::create_dir_all(skills_dir).map_err(|e| format!("创建 skills 目录失败: {e}"))?;

    let mut args: Vec<&str> = vec!["clone", "--depth", "1"];
    if let Some(b) = branch.filter(|b| !b.trim().is_empty()) {
        args.push("--branch");
        args.push(b);
    }
    args.push(url);
    let target_str = target.to_string_lossy().to_string();
    args.push(&target_str);
    if let Err(e) = sh(&args) {
        // 失败即清场：clone 中途失败/被杀会留下半成品目录（网络不可达时连
        // `.git` 都在），不清理则本次与后续重试都会被「目录已存在」挡住。
        let _ = std::fs::remove_dir_all(&target);
        return Err(e);
    }

    // 目录提升：根无 SKILL.md 时，取第一个子目录作为根
    if !target.join("SKILL.md").exists() {
        if let Some(sub) = find_skill_root(&target) {
            // 拷贝子目录内容到 root，然后清掉子目录
            for entry in std::fs::read_dir(&sub)
                .map_err(|e| e.to_string())?
                .flatten()
            {
                let src = entry.path();
                let dst = target.join(entry.file_name());
                if dst.exists() {
                    let _ = std::fs::remove_dir_all(&dst);
                }
                std::fs::rename(&src, &dst).map_err(|e| format!("提升 skill 内容失败: {e}"))?;
            }
            let _ = std::fs::remove_dir_all(&sub);
        }
    }
    if !target.join("SKILL.md").exists() {
        // 清理半成品目录
        let _ = std::fs::remove_dir_all(&target);
        return Err(format!("仓库 {url} 中未找到 SKILL.md"));
    }

    let rev = sh(&["-C", &target.to_string_lossy(), "rev-parse", "HEAD"])
        .unwrap_or_else(|_| "unknown".into());
    // 记录键 = 技能名（frontmatter `name`），不是目录名：`emit_skills_list`
    // 按 `skill.metadata.name` 反查来源，键不一致则「更新 / 移除来源」在
    // Panel 里整体不可见（2026-10-18 实测：目录 skill-echo-prodoc /
    // 技能 prodoc-format）。读不到 frontmatter 时回退目录名。
    let skill_name = crate::loader::load_skill(&target.join("SKILL.md"))
        .ok()
        .map(|skill| skill.metadata.name.trim().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| dir_name.clone());
    let source = SkillSource {
        url: url.to_string(),
        rev,
        branch: branch.map(|b| b.to_string()),
        installed_at: Some(chrono::Utc::now().to_rfc3339()),
        dir: Some(dir_name.clone()),
    };

    let mut sources = load_sources(skills_dir);
    sources.insert(skill_name, source.clone());
    save_sources(skills_dir, &sources)?;
    Ok(source)
}

/// 更新：`git fetch --depth 1` + `git reset --hard origin/<branch||HEAD>`。
pub fn update(skills_dir: &Path, name: &str) -> Result<SkillSource, String> {
    let mut sources = load_sources_healed(skills_dir);
    let source = sources
        .get(name)
        .cloned()
        .ok_or_else(|| format!("{name} 不是 Git 来源安装的 skill（无来源记录）"))?;
    // 目录名可能与技能名不同（见 `SkillSource::dir`）；旧记录无该字段 →
    // 键即目录名。
    let dir_name = source.dir.clone().unwrap_or_else(|| name.to_string());
    let target = skills_dir.join(&dir_name);
    if !target.join("SKILL.md").exists() {
        return Err(format!("目录不存在: {dir_name}"));
    }
    let target_str = target.to_string_lossy().to_string();
    sh(&["-C", &target_str, "fetch", "--depth", "1", "origin"])?;
    let refspec = match &source.branch {
        Some(b) => format!("origin/{b}"),
        None => "origin/HEAD".to_string(),
    };
    // 尝试 reset 到指定 ref，失败回退到 origin 默认分支
    let reset = sh(&["-C", &target_str, "reset", "--hard", &refspec]);
    if reset.is_err() {
        sh(&["-C", &target_str, "reset", "--hard", "origin/HEAD"])?;
    }
    let rev = sh(&["-C", &target_str, "rev-parse", "HEAD"]).unwrap_or_else(|_| "unknown".into());
    let mut updated = source.clone();
    updated.rev = rev;
    updated.installed_at = Some(chrono::Utc::now().to_rfc3339());
    sources.insert(name.to_string(), updated.clone());
    save_sources(skills_dir, &sources)?;
    Ok(updated)
}

/// 移除来源记录（不删目录）。
pub fn remove_source(skills_dir: &Path, name: &str) -> Result<(), String> {
    let mut sources = load_sources_healed(skills_dir);
    if sources.remove(name).is_none() {
        return Err(format!("{name} 无 Git 来源记录"));
    }
    save_sources(skills_dir, &sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时 skills 目录（唯一名，Drop 即清理）。
    struct TempSkills(PathBuf);
    impl TempSkills {
        fn new() -> Self {
            let dir =
                std::env::temp_dir().join(format!("echo-skills-{}", uuid::Uuid::new_v4().simple()));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempSkills {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn broken_clone_is_only_git_without_skill_md() {
        let skills = TempSkills::new();
        // 只有 .git：失败残留
        let broken = skills.path().join("broken");
        std::fs::create_dir_all(broken.join(".git")).unwrap();
        assert!(is_broken_clone(&broken));

        // 空目录（连 .git 都没有）：不是 clone 残留，按"已存在"处理
        let empty = skills.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(!is_broken_clone(&empty));

        // 有 SKILL.md：正常技能目录，绝不能被判成残留（否则会误删用户数据）
        let good = skills.path().join("good");
        std::fs::create_dir_all(good.join(".git")).unwrap();
        std::fs::write(good.join("SKILL.md"), "---\nname: x\n---\n").unwrap();
        assert!(!is_broken_clone(&good));
    }

    /// clone 失败必须清场：否则残留目录会让**同一名字**永久装不上
    /// （2026-10-18 实况：HTTPS 不可达 → 120s 超时 → `skills/<name>/.git`
    /// 残留 → 重试报「目录已存在」，用户完全无法自助恢复）。
    #[test]
    fn failed_clone_leaves_no_directory() {
        let skills = TempSkills::new();
        // 不存在的本地路径：git 立即失败，不触网。
        let bogus = skills.path().join("no-such-repo-anywhere");
        let url = format!("file://{}", bogus.display());
        let err = install(skills.path(), &url, Some("target"), None).unwrap_err();
        assert!(
            err.contains("git clone 失败"),
            "错误信息应说明 clone 失败: {err}"
        );
        assert!(
            !skills.path().join("target").exists(),
            "失败后不得留下半成品目录"
        );
    }

    /// 残留目录自动清掉后再装（此处仍失败，但错误必须是 clone 失败而不是
    /// 「目录已存在」——证明残留被识别并清理）。
    #[test]
    fn stale_broken_clone_is_cleaned_before_retry() {
        let skills = TempSkills::new();
        let target = skills.path().join("target");
        std::fs::create_dir_all(target.join(".git").join("refs")).unwrap();
        assert!(target.exists());

        let url = format!("file://{}", skills.path().join("missing-repo").display());
        let err = install(skills.path(), &url, Some("target"), None).unwrap_err();
        assert!(
            !err.contains("目录已存在"),
            "残留应被自动清理，而不是撞「目录已存在」: {err}"
        );
        assert!(err.contains("git clone 失败"), "{err}");
        assert!(!target.exists(), "本次失败同样要清场");
    }

    /// 正常技能目录（有 SKILL.md）不被当成残留：拒绝覆盖并给出指引。
    #[test]
    fn existing_real_skill_is_not_overwritten() {
        let skills = TempSkills::new();
        let target = skills.path().join("taken");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("SKILL.md"), "---\nname: taken\n---\n").unwrap();

        let url = format!("file://{}", skills.path().join("whatever").display());
        let err = install(skills.path(), &url, Some("taken"), None).unwrap_err();
        assert!(err.contains("目录已存在"), "{err}");
        // 用户数据原封不动
        assert!(target.join("SKILL.md").exists());
    }

    /// 来源记录键必须是**技能名**（frontmatter `name`），否则 Panel 的
    /// 「更新 / 移除来源」在「目录名 ≠ 技能名」的仓库上整体失踪
    /// （2026-10-18 实测：目录 `skill-echo-prodoc`，技能 `prodoc-format`）。
    ///
    /// 这里用本地 git 仓库（不触网）复现那个形状。
    #[test]
    fn sources_key_is_skill_name_not_directory_name() {
        let skills = TempSkills::new();
        let repo = TempSkills::new();
        // 造一个只有 SKILL.md 的本地 git 仓库
        std::fs::write(
            repo.path().join("SKILL.md"),
            "---\nname: prodoc-format\ndescription: 规范\n---\n# 正文\n",
        )
        .unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .output()
                .expect("git");
            assert!(status.status.success(), "git {args:?}: {:?}", status);
        };
        git(&["init", "-q"]);
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);

        let url = repo.path().to_string_lossy().to_string();
        let src = install(skills.path(), &url, Some("skill-echo-prodoc"), None).unwrap();

        // 记录键 = prodoc-format（frontmatter name），dir = 安装目录
        assert_eq!(src.dir.as_deref(), Some("skill-echo-prodoc"));
        let sources = load_sources(skills.path());
        assert!(
            sources.contains_key("prodoc-format"),
            "键应为技能名: {:?}",
            sources.keys().collect::<Vec<_>>()
        );
        assert!(
            !sources.contains_key("skill-echo-prodoc"),
            "不应以目录名为键"
        );
        // 目录确实叫 skill-echo-prodoc
        assert!(skills
            .path()
            .join("skill-echo-prodoc")
            .join("SKILL.md")
            .exists());

        // update 能用技能名找到目录（旧实现会报「目录不存在」）
        let updated = update(skills.path(), "prodoc-format").unwrap();
        assert_eq!(updated.dir.as_deref(), Some("skill-echo-prodoc"));

        // remove_source 同样按技能名
        remove_source(skills.path(), "prodoc-format").unwrap();
        assert!(load_sources(skills.path()).is_empty());
    }

    /// 旧格式记录（无 `dir` 字段）仍可更新：键即目录名。
    #[test]
    fn legacy_source_without_dir_updates_by_key() {
        let skills = TempSkills::new();
        let name = "legacy-skill";
        let target = skills.path().join(name);
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("SKILL.md"), "---\nname: legacy-skill\n---\n").unwrap();
        // 手写旧格式记录
        let raw = format!(
            r#"{{"{name}":{{"url":"file:///nowhere","rev":"abc","branch":null,"installed_at":null}}}}"#
        );
        std::fs::write(sources_path(skills.path()), raw).unwrap();

        let sources = load_sources(skills.path());
        assert_eq!(sources[name].dir, None, "旧记录无 dir");
        // update 会先 fetch 失败（本地无 origin）——但错误必须是 git 失败，
        // 不能是「目录不存在」（后者说明目录定位错了）。
        let err = update(skills.path(), name).unwrap_err();
        assert!(
            err.contains("git"),
            "应按 dir 回退定位目录后走到 git: {err}"
        );
    }

    /// 旧记录归正：键=目录名、无 dir → 内存里改挂到技能名 + 补 dir。
    #[test]
    fn heal_moves_legacy_key_to_skill_name() {
        let skills = TempSkills::new();
        // 目录名 skill-echo-prodoc，技能名 prodoc-format
        let dir = skills.path().join("skill-echo-prodoc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: prodoc-format\ndescription: x\n---\n",
        )
        .unwrap();
        std::fs::write(
            sources_path(skills.path()),
            r#"{"skill-echo-prodoc":{"url":"git@github.com:EchoLab-Auto/skill-echo-prodoc.git","rev":"f727ad5"}}"#,
        )
        .unwrap();

        let healed = load_sources_healed(skills.path());
        assert!(healed.contains_key("prodoc-format"), "应改挂到技能名");
        assert!(!healed.contains_key("skill-echo-prodoc"), "旧键不应保留");
        assert_eq!(
            healed["prodoc-format"].dir.as_deref(),
            Some("skill-echo-prodoc")
        );
        assert_eq!(healed["prodoc-format"].rev, "f727ad5", "其余字段原样保留");

        // 归正只在内存：磁盘上仍是旧键（写入留给 install/update/remove）
        assert!(load_sources(skills.path()).contains_key("skill-echo-prodoc"));
    }

    /// 归正后 update 能按技能名找到真实目录。
    #[test]
    fn healed_legacy_source_can_be_updated_by_skill_name() {
        let skills = TempSkills::new();
        let dir = skills.path().join("skill-echo-prodoc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), "---\nname: prodoc-format\n---\n").unwrap();
        std::fs::write(
            sources_path(skills.path()),
            r#"{"skill-echo-prodoc":{"url":"file:///nowhere","rev":"old"}}"#,
        )
        .unwrap();

        // 目录定位正确 → 走到 git fetch 才失败（不是「无来源记录」/「目录不存在」）
        let err = update(skills.path(), "prodoc-format").unwrap_err();
        assert!(!err.contains("无来源记录"), "{err}");
        assert!(!err.contains("目录不存在"), "{err}");
        assert!(err.contains("git"), "{err}");
    }

    #[test]
    fn repo_name_strips_git_suffix_and_trailing_slash() {
        assert_eq!(repo_name("https://github.com/a/b.git"), "b");
        assert_eq!(repo_name("https://github.com/a/b/"), "b");
        assert_eq!(repo_name("git@github.com:a/b.git"), "b");
        assert_eq!(repo_name("   "), "   ".trim_end_matches('/'));
    }
}
