//! Guard: 出厂技能层（`skills-builtin/`）的**反向清理**必须真的能删掉陈旧技能。
//!
//! ## 背景（2026-11，真实事故）
//!
//! 技能的同步只做了「正向」：把新检出里的 `skills/*/` 逐个刷到只读层。
//! 源里**删掉**的技能进不了那个循环，于是旧部署的只读层永远留着它——
//! ai_station 容器实测：QQ / subagent / prodoc 的四个技能已迁给各自模块，
//! 容器里连 QQ 模块都没装，Panel 却照旧列出并触发它们，存活了 9 天。
//!
//! ## 为什么是行为测试而不是文本守护
//!
//! 这段逻辑的失效方式是**静默**的（技能目录还在、发现逻辑照旧加载它），
//! 光断言"脚本里有这段代码"挡不住"判据写错"。所以这里把两个脚本里的
//! 清理循环**原文抽出来执行**，在临时目录上验证四条语义：
//!
//! | 情形 | 期望 |
//! |---|---|
//! | 源里还有的同名技能 | 保留，内容更新为新版 |
//! | 源里已删、目录里有 SKILL.md | 删掉 |
//! | 源里已删、里面有**嵌套** SKILL.md | 删掉（递归发现也会加载它） |
//! | 源里已删、但没有 SKILL.md（配置/备份残留） | 保留（不归这里管） |

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("modules/")
        .parent()
        .expect("source/")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read_script(name: &str) -> String {
    let path = repo_root().join("scripts").join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// 从脚本里抽出「反向清理」循环的原文。
///
/// 定位靠两端的锚行（`for _skill_dst in` … `unset _skill_dst`）：这段代码在
/// install.sh 与 update.sh 里是**逐字相同**的（两条安装路径必须同语义），
/// 抽出来执行等于测的就是线上那一份。
fn prune_snippet(script: &str) -> String {
    let lines: Vec<&str> = script.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.trim_start().starts_with("for _skill_dst in"))
        .expect("反向清理循环缺失（陈旧的出厂技能会永远留着）");
    let end = lines[start..]
        .iter()
        .position(|line| line.trim() == "unset _skill_dst _skill_name")
        .map(|offset| start + offset)
        .expect("反向清理循环少了收尾的 unset");
    lines[start..=end].join("\n")
}

fn write(path: &Path, body: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, body).unwrap();
}

/// 在临时目录里跑一段清理循环；返回保留下来的顶层目录名（排序后）。
fn run_prune(snippet: &str, tag: &str) -> (Vec<String>, String) {
    let root = std::env::temp_dir().join(format!(
        "echo-factory-skills-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let source_skills = root.join("source").join("skills");
    let builtin = root.join("builtin");

    // 源里现存的技能：alpha（改名内容）= 该更新；beta = 保持
    write(&source_skills.join("alpha").join("SKILL.md"), "# alpha v2\n");
    write(&source_skills.join("beta").join("SKILL.md"), "# beta\n");
    // 只读层的历史状态：alpha 是旧版；另有三种残留
    write(&builtin.join("alpha").join("SKILL.md"), "# alpha v1\n");
    write(&builtin.join("beta").join("SKILL.md"), "# beta\n");
    write(&builtin.join("stale-skill").join("SKILL.md"), "# 已从仓库移除\n");
    write(
        &builtin.join("nested").join("inner").join("SKILL.md"),
        "# 嵌套的陈旧技能\n",
    );
    write(&builtin.join("not-a-skill").join("config.json"), "{}\n");

    // 正向同步（与脚本同语义：cp -a 不删任何东西）
    let status = Command::new("cp")
        .arg("-a")
        .arg(format!("{}/.", source_skills.display()))
        .arg(&builtin)
        .status()
        .expect("cp -a");
    assert!(status.success(), "cp -a 失败");

    let output = Command::new("bash")
        .arg("-c")
        .arg(format!("set -euo pipefail\n{snippet}"))
        .env("SOURCE_DIR", root.join("source"))
        .env("BUILTIN_SKILLS_DIR", &builtin)
        .output()
        .expect("run prune snippet");
    assert!(
        output.status.success(),
        "清理循环执行失败：{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut kept: Vec<String> = std::fs::read_dir(&builtin)
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry
                .file_type()
                .unwrap()
                .is_dir()
                .then(|| entry.file_name().to_string_lossy().into_owned())
        })
        .collect();
    kept.sort();
    let alpha = std::fs::read_to_string(builtin.join("alpha").join("SKILL.md")).unwrap_or_default();
    let _ = std::fs::remove_dir_all(&root);
    (kept, alpha)
}

/// 两个脚本必须都能删掉陈旧技能、保留仍在源的技能、更新同名内容。
fn assert_prune_semantics(script_name: &str) {
    let script = read_script(script_name);
    let snippet = prune_snippet(&script);
    let (kept, alpha) = run_prune(&snippet, script_name);
    assert_eq!(
        kept,
        vec![
            "alpha".to_string(),
            "beta".to_string(),
            "not-a-skill".to_string(),
        ],
        "{script_name}: 期望只保留「源里仍有的」+「不含 SKILL.md 的残留」，实际 {kept:?}"
    );
    assert_eq!(
        alpha, "# alpha v2\n",
        "{script_name}: 源里仍存在的技能内容必须被更新"
    );
}

#[test]
fn update_script_prunes_stale_factory_skills() {
    assert_prune_semantics("update.sh");
}

#[test]
fn install_script_prunes_stale_factory_skills() {
    assert_prune_semantics("install.sh");
}

/// 两份脚本的清理循环必须**逐字相同**：install 与自更新是两条并行路径，
/// 判据分叉的后果是「全新装好的机器没事、升级上来的机器带残留」这种
/// 只在老部署复现的诡异差异。
#[test]
fn both_scripts_share_the_same_prune_rule() {
    // 缩进不参与比较：update.sh 里这段套在 `if` 内（4 空格），install.sh 里
    // 是顶层（无缩进）。要守的是**判据**一致，不是排版一致。
    let normalize = |script: &str| -> Vec<String> {
        prune_snippet(script)
            .lines()
            .map(|line| line.trim_start().to_string())
            .collect()
    };
    assert_eq!(
        normalize(&read_script("update.sh")),
        normalize(&read_script("install.sh")),
        "update.sh 与 install.sh 的出厂技能清理判据已经分叉"
    );
}
