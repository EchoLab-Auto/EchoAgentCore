//! 外部 Git 来源 skill 的安装/更新管理。
//!
//! - 安装：`git clone` 到 `skills_dir/<name>`（SKILL.md 热重载自动发现）；
//!   clone 后若仓库根没有 SKILL.md，尝试查找子目录中的 SKILL.md 并提升
//!   为安装根（把子目录内容拷到安装目录）。
//! - 来源记录：`skills_dir/.sources.json`（{ name: { url, rev, branch, installed_at } }），
//!   供更新（fetch + reset 到记录 rev/branch）与前端来源徽标展示。
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

fn sh(args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|e| format!("git 执行失败: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("git {} 失败: {}", args[0], err));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
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
        return Err(format!("目录已存在: {dir_name}（请先卸载或换名安装）"));
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
    sh(&args)?;

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
    let source = SkillSource {
        url: url.to_string(),
        rev,
        branch: branch.map(|b| b.to_string()),
        installed_at: Some(chrono::Utc::now().to_rfc3339()),
    };

    let mut sources = load_sources(skills_dir);
    sources.insert(dir_name.clone(), source.clone());
    save_sources(skills_dir, &sources)?;
    Ok(source)
}

/// 更新：`git fetch --depth 1` + `git reset --hard origin/<branch||HEAD>`。
pub fn update(skills_dir: &Path, name: &str) -> Result<SkillSource, String> {
    let mut sources = load_sources(skills_dir);
    let source = sources
        .get(name)
        .cloned()
        .ok_or_else(|| format!("{name} 不是 Git 来源安装的 skill（无来源记录）"))?;
    let target = skills_dir.join(name);
    if !target.join("SKILL.md").exists() {
        return Err(format!("目录不存在: {name}"));
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
    let mut sources = load_sources(skills_dir);
    if sources.remove(name).is_none() {
        return Err(format!("{name} 无 Git 来源记录"));
    }
    save_sources(skills_dir, &sources)
}
