//! `read_skill` 工具：按需读取技能完整正文。
//!
//! # 为什么需要它
//!
//! 技能正文过去全部内联进系统提示——而系统提示**每一步模型往返都会重发
//! 一次**。本机实测四个常驻技能合计 24 KB，其中一个单文件 18.5 KB；一个
//! 跑 30 步的 turn 就是 30 × 24 KB 的输入。
//!
//! 索引化之后（见 [`crate::prompt::inline_decision`]），长技能只在提示里留
//! 一段索引与开头预览，完整正文改由本工具按需读取：代价从"每步重发"降到
//! "一次读取"，而且只在真的需要细节时才付。
//!
//! # 为什么从磁盘读，而不是从提示词块里取
//!
//! 提示词块里的技能正文归 `SkillsRuntime`（**per-persona** 的 `SkillRegistry`），
//! 工具注册在宿主的全局注册表里，两者没有共享句柄。走「工具 → 模块命令」
//! 又要把 `ReadSkill` 塞进 `BackendCommand` 协议。而技能正文的**源头**就是
//! 磁盘上的 `SKILL.md`：`discover_many` 是既有的分层发现入口（出厂层在前、
//! 用户层在后，同名覆盖语义与启动时一致），每次执行重扫一遍不过是几十个
//! 小文件的 stat + 解析——比多一条跨模块协议值得。
//!
//! 代价如实记录：**模块贡献的技能**（`include_str!` 进二进制、磁盘上没有
//! 文件）本工具读不到，会明确报错而不是给一个空正文。
//!
//! # 分页
//!
//! 技能正文可以很长（`long-task-efficiency` 18.5 KB ≈ 6k token）。
//! `offset` / `limit` 按**字符**（不是字节）切，避免切断多字节字符；默认
//! 一次给 8000 字符，上限 20000。返回尾部带下一页的 offset，模型可以自己
//! 续读——不用猜"是不是还有"。

use echo_defs::tool::{Tool, ToolError};
use serde_json::{json, Value};

/// 工具名（宿主注册与提示词里的读取指引都引用它）。
pub const TOOL_NAME: &str = "read_skill";

/// 单次默认返回字符数。
pub const DEFAULT_LIMIT: usize = 8_000;
/// 单次上限（防止一次把整份技能塞回上下文，那就又变回内联了）。
pub const MAX_LIMIT: usize = 20_000;

pub struct ReadSkillTool {
    /// 技能目录（出厂层在前、用户层在后；与启动时同一份清单）。
    dirs: Vec<String>,
}

impl ReadSkillTool {
    pub fn new(dirs: Vec<String>) -> Self {
        Self { dirs }
    }
}

#[async_trait::async_trait]
impl Tool for ReadSkillTool {
    fn name(&self) -> &str {
        TOOL_NAME
    }

    fn description(&self) -> &str {
        "读取一个技能的完整正文。系统提示里的长技能只内联索引与开头预览，\
         需要完整规则（流程、清单、命令示例）时用本工具按名称读取；\
         正文较长时用 offset/limit 按字符分页续读。"
    }

    fn category(&self) -> &'static str {
        "builtin"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "技能名（系统提示的技能清单里列出的名称）"
                },
                "offset": {
                    "type": "integer",
                    "description": "起始字符偏移（默认 0；续读时用上次结果给出的 offset）"
                },
                "limit": {
                    "type": "integer",
                    "description": format!("本次最多返回字符数（默认 {DEFAULT_LIMIT}，上限 {MAX_LIMIT}）")
                }
            },
            "required": ["name"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        let name = arguments
            .get("name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("缺少 name（技能名）".into()))?
            .to_string();
        let offset = arguments
            .get("offset")
            .and_then(|v| v.as_u64())
            .unwrap_or(0) as usize;
        let limit = arguments
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, MAX_LIMIT);

        // 目录扫描是同步 IO：放到 blocking 线程，别在 async 执行器上跑
        // walkdir（技能多时会把整个世界卡住一秒）。
        let dirs = self.dirs.clone();
        let wanted = name.clone();
        let skill = tokio::task::spawn_blocking(move || {
            crate::SkillRegistry::discover_many(&dirs)
                .ok()
                .and_then(|registry| registry.get(&wanted).cloned())
        })
        .await
        .map_err(|error| ToolError::Execution(format!("技能目录读取失败：{error}")))?;

        let Some(skill) = skill else {
            return Err(ToolError::NotFound(format!(
                "{name}（磁盘技能目录里没有这个名字；若是模块内置技能，其正文已在系统提示中）"
            )));
        };
        Ok(render_slice(&skill, offset, limit))
    }
}

/// 把技能正文按字符切一段，附上说明与续读指引。
fn render_slice(skill: &crate::Skill, offset: usize, limit: usize) -> String {
    let name = &skill.metadata.name;
    let total = skill.instructions.chars().count();
    if total == 0 {
        return format!("# {name}\n（该技能正文为空）");
    }
    let body: String = skill.instructions.chars().skip(offset).take(limit).collect();
    let end = (offset + body.chars().count()).min(total);

    let mut out = String::new();
    if offset == 0 {
        if !skill.metadata.description.trim().is_empty() {
            out.push_str(&format!("> {}\n\n", skill.metadata.description));
        }
    } else {
        out.push_str(&format!("（续读：{name} 字符 {offset}–{end} / 共 {total}）\n\n"));
    }
    out.push_str(&body);
    // 只在确实还有剩余时给续读指引，避免模型对着完整正文反复续读。
    if end < total {
        out.push_str(&format!(
            "\n\n…（还有 {remaining} 字符；续读：read_skill(name=\"{name}\", offset={end})）",
            remaining = total - end
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn skill(body: &str) -> crate::Skill {
        crate::Skill {
            metadata: crate::SkillMetadata {
                name: "demo".into(),
                description: "演示技能".into(),
                keywords: vec![],
                always: true,
                system: false,
                enabled: true,
                category: String::new(),
                package: None,
            },
            instructions: body.into(),
        }
    }

    /// 在临时目录里落一份真的 `SKILL.md`（工具读的是磁盘）。
    fn temp_skill_dir(tag: &str, body: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "echo-read-skill-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("demo")).unwrap();
        std::fs::write(
            dir.join("demo/SKILL.md"),
            format!("---\nname: demo\ndescription: 演示技能\n---\n\n{body}"),
        )
        .unwrap();
        dir
    }

    #[test]
    fn returns_full_body_and_description() {
        let out = render_slice(&skill("完整正文"), 0, DEFAULT_LIMIT);
        assert!(out.contains("演示技能"));
        assert!(out.contains("完整正文"));
        // 正文完整返回时不给续读指引（否则模型会反复读同一段）。
        assert!(!out.contains("续读"));
    }

    #[test]
    fn paginates_by_characters_not_bytes() {
        // 中文多字节：按字节切会切成乱码。
        let out = render_slice(&skill(&"中".repeat(100)), 0, 10);
        assert!(out.contains("续读"), "must advertise the next page: {out}");
        assert_eq!(out.matches('中').count(), 10, "limit is char-based");
        assert!(out.contains("offset=10"), "{out}");
    }

    #[tokio::test]
    async fn reads_skill_from_disk_and_reports_unknown_names() {
        let dir = temp_skill_dir("disk", "磁盘上的正文");
        let tool = ReadSkillTool::new(vec![dir.display().to_string()]);
        let out = tool.execute(json!({"name": "demo"})).await.expect("found");
        assert!(out.contains("磁盘上的正文"), "{out}");

        let error = tool
            .execute(json!({"name": "nope"}))
            .await
            .expect_err("must fail");
        assert!(format!("{error}").contains("nope"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_name_is_invalid_arguments() {
        let tool = ReadSkillTool::new(Vec::new());
        assert!(matches!(
            tool.execute(json!({})).await,
            Err(ToolError::InvalidArguments(_))
        ));
    }
}
