//! System prompt construction: named blocks for token-usage visualization.
//!
//! Builds the system prompt from layers (base, system skills, persona skills,
//! skill list, always-on skills, triggered skills, workspace, boundary rules)
//! as named blocks, then joins them for the LLM request.

use super::boundary::{BoundaryKind, PromptBlock};

/// Build the system prompt as named blocks so the panel can visualize
/// token usage per section.
///
/// `module_blocks` 是**已挂载模块贡献的有序文本块**（
/// `Contribution::Text`；由 `ModuleHost::collect_text_blocks` 收集，已按 order
/// 排好序、已丢弃空内容）。
///
/// 为什么不再是 `workspace: Option<String>`：那是"某个模块专属内容"作为位置
/// 参数写进装配函数——每来一个需要注入内容的模块就要改一次签名。现在装配与
/// 模块数量解耦：模块在不在、有几个，这个函数都不用改。
///
/// 模块的块的 `kind` 取它的 `key`（如 `workspace`），所以面板的种类徽标与
/// 排序语义保持不变。
pub(crate) async fn build_prompt_blocks(
    base_prompt: &str,
    boundary: Option<BoundaryKind>,
    module_blocks: &[(echo_module::ModuleTextBlock, String)],
    contributed_blocks: Vec<PromptBlock>,
) -> Vec<PromptBlock> {
    let mut blocks = Vec::new();
    blocks.push(PromptBlock {
        key: "base".into(),
        label: "系统提示词".into(),
        kind: "base".into(),
        content: base_prompt.to_string(),
    });
    blocks.extend(contributed_blocks);
    // 模块贡献的文本块（`Contribution::Text`）：按 order 排好序、空内容已丢。
    //
    // 位置在"触发技能"之后、"边界"之前——与历史行为一致（工作区会话提示词
    // 原本就在这里）。`kind` 取块的 `key`，面板的种类徽标因此不变。
    for (block, text) in module_blocks {
        blocks.push(PromptBlock {
            key: block.key.clone(),
            label: if block.label.is_empty() {
                block.key.clone()
            } else {
                block.label.clone()
            },
            kind: block.key.clone(),
            content: text.clone(),
        });
    }
    if let Some(boundary) = boundary {
        blocks.push(boundary.block());
    }
    blocks
}

/// Join prompt blocks with the same separator the original string builder used.
pub(crate) fn join_prompt_blocks(blocks: &[PromptBlock]) -> String {
    blocks
        .iter()
        .map(|block| block.content.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}
