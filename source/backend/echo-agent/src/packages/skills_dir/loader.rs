//! SKILL.md parser: YAML-ish frontmatter + markdown body.
//!
//! Frontmatter is a small subset of YAML — line-oriented `key: value` and
//! `key: [a, b]` lists — enough for skill definitions without pulling in a
//! YAML crate.

use std::path::Path;

use crate::skill::{Skill, SkillError, SkillMetadata};

/// Parse a SKILL.md file.
///
/// ```markdown
/// ---
/// name: web-search
/// description: 搜索网页获取最新信息
/// keywords: [天气, 新闻, 搜索]
/// ---
/// # 技能说明
/// ...
/// ```
pub fn load_skill(path: &Path) -> Result<Skill, SkillError> {
    let text = std::fs::read_to_string(path).map_err(|e| SkillError::Load {
        path: path.display().to_string(),
        err: e.to_string(),
    })?;
    parse_skill(path, &text)
}

fn parse_skill(path: &Path, text: &str) -> Result<Skill, SkillError> {
    let (frontmatter, body) = split_frontmatter(text);
    let (
        mut name,
        mut description,
        mut keywords,
        mut always,
        mut category,
        mut package,
        mut system,
    ) = (
        String::new(),
        String::new(),
        Vec::new(),
        false,
        String::new(),
        None,
        false,
    );
    let mut in_metadata = false;
    for raw_line in frontmatter.lines() {
        let is_indented = raw_line.starts_with([' ', '\t']);
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if !is_indented {
            in_metadata = key == "metadata";
        }
        match key {
            "name" => name = value.trim_matches('"').trim_matches('\'').to_string(),
            "description" => description = value.trim_matches('"').trim_matches('\'').to_string(),
            "keywords" => {
                keywords = parse_list(value);
            }
            "category" => category = value.trim_matches('"').trim_matches('\'').to_string(),
            "package" => {
                let v = value.trim_matches('"').trim_matches('\'');
                package = if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                };
            }
            "always" if in_metadata => always = value.eq_ignore_ascii_case("true"),
            "system" => system = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    if name.is_empty() {
        name = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("unnamed")
            .to_string();
    }
    Ok(Skill {
        metadata: SkillMetadata {
            name,
            description,
            keywords,
            always,
            system,
            enabled: true,
            category,
            package,
        },
        instructions: body.trim().to_string(),
    })
}

/// Split `--- frontmatter ---` from the body.
fn split_frontmatter(text: &str) -> (&str, &str) {
    let trimmed = text.trim_start_matches('\u{feff}');
    let Some(rest) = trimmed.strip_prefix("---") else {
        return ("", trimmed);
    };
    let rest = rest.strip_prefix('\n').unwrap_or(rest);
    // find the closing `---` line
    let mut lines = rest.splitn(2, "\n---");
    match (lines.next(), lines.next()) {
        (Some(fm), Some(body)) => (fm, body),
        _ => ("", trimmed),
    }
}

/// Parse `[a, b, c]` or `a, b, c` into trimmed strings.
fn parse_list(value: &str) -> Vec<String> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter_and_body() {
        let text = r#"---
name: web-search
description: 搜索网页获取最新信息
keywords: [天气, 新闻, "搜索"]
---
# 使用说明
需要搜索时调用 web_search 工具。"#;
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.name, "web-search");
        assert_eq!(skill.metadata.keywords, vec!["天气", "新闻", "搜索"]);
        assert!(skill.instructions.contains("web_search"));
    }

    #[test]
    fn no_frontmatter_uses_dir_name() {
        let skill = parse_skill(Path::new("/skills/calc/SKILL.md"), "# 计算器").unwrap();
        assert_eq!(skill.metadata.name, "calc");
    }

    #[test]
    fn split_works() {
        let (fm, body) = split_frontmatter("---\na: 1\n---\nbody text");
        assert_eq!(fm.trim(), "a: 1");
        assert_eq!(body.trim(), "body text");
        let (fm, body) = split_frontmatter("no frontmatter");
        assert_eq!(fm, "");
        assert_eq!(body, "no frontmatter");
    }

    #[test]
    fn empty_file_falls_back_to_dir_name() {
        let skill = parse_skill(Path::new("/skills/empty-skill/SKILL.md"), "").unwrap();
        assert_eq!(skill.metadata.name, "empty-skill");
        assert!(skill.instructions.is_empty());
    }

    #[test]
    fn bom_prefix_is_ignored() {
        let text = "\u{feff}---\nname: bom-skill\n---\nbody";
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.name, "bom-skill");
    }

    #[test]
    fn quoted_values_are_unquoted() {
        let text = "---\nname: \"double\"\ndescription: 'single'\n---\n";
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.name, "double");
        assert_eq!(skill.metadata.description, "single");
    }

    #[test]
    fn keywords_without_brackets_are_supported() {
        let text = "---\nname: k\nkeywords: 天气, 新闻\n---\n";
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.keywords, vec!["天气", "新闻"]);
    }

    #[test]
    fn unterminated_frontmatter_is_treated_as_body() {
        // Opening `---` with no closing delimiter → no frontmatter at all.
        let text = "---\nname: orphan\nbody text";
        let skill = parse_skill(Path::new("/skills/orphan/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.name, "orphan", "falls back to dir name");
        assert!(skill.metadata.description.is_empty());
    }

    #[test]
    fn parses_category_metadata() {
        let text = r#"---
name: web-search
description: 搜索网页获取最新信息
keywords: [天气, 新闻]
metadata:
  category: 信息检索
---
使用说明"#;
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.category, "信息检索");
    }

    #[test]
    fn missing_category_defaults_to_empty() {
        let skill = parse_skill(Path::new("/x/SKILL.md"), "---\nname: plain\n---\nbody").unwrap();
        assert_eq!(skill.metadata.category, "");
    }

    #[test]
    fn unknown_keys_and_comments_are_ignored() {
        let text = "---\nname: k\nversion: 1.0\n# a comment\nunknown: x\n---\nbody";
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.name, "k");
        assert!(skill.metadata.description.is_empty());
    }

    #[test]
    fn crlf_line_endings_parse() {
        let text = "---\r\nname: crlf-skill\r\ndescription: ok\r\nkeywords: [a, b]\r\n---\r\nbody";
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.name, "crlf-skill");
        assert_eq!(skill.metadata.keywords, vec!["a", "b"]);
    }

    #[test]
    fn colon_in_value_is_preserved() {
        let text = "---\ndescription: 使用 https://example.com 搜索\n---\n";
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert_eq!(skill.metadata.description, "使用 https://example.com 搜索");
    }

    #[test]
    fn parses_always_from_metadata() {
        let text = "---\nname: concise\ndescription: d\nmetadata:\n  always: true\n---\nbody";
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert!(skill.metadata.always);
    }

    #[test]
    fn ignores_top_level_always() {
        let text = "---\nname: concise\nalways: true\n---\nbody";
        let skill = parse_skill(Path::new("/x/SKILL.md"), text).unwrap();
        assert!(!skill.metadata.always);
    }

    /// 真实技能文件的冒烟校验（人工触发）：
    ///
    /// ```text
    /// ECHO_SKILLS_DIR=<仓库>/skills \
    ///   cargo test -p echo-agent --lib smoke_skills_dir -- --ignored --nocapture
    /// ```
    ///
    /// 把目录里每个 `SKILL.md` 过一遍真实解析器并打印关键字段——改完技能
    /// 文件、在面板点「重载技能」之前先跑一次，避免坏 frontmatter 上生产。
    #[test]
    #[ignore = "manual: set ECHO_SKILLS_DIR to a skills directory"]
    fn smoke_skills_dir_parses_real_files() {
        let dir = std::env::var("ECHO_SKILLS_DIR").expect("set ECHO_SKILLS_DIR");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&dir).expect("read skills dir") {
            let entry = entry.expect("dir entry");
            let file = entry.path().join("SKILL.md");
            if !file.exists() {
                continue;
            }
            let skill =
                load_skill(&file).unwrap_or_else(|error| panic!("{}: {error}", file.display()));
            eprintln!(
                "{}: always={} system={} keywords={:?}",
                skill.metadata.name,
                skill.metadata.always,
                skill.metadata.system,
                skill.metadata.keywords
            );
            checked += 1;
        }
        assert!(checked > 0, "no SKILL.md found under {dir}");
    }
}
