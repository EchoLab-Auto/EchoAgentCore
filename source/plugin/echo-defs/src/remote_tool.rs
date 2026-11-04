//! 远程工具命名（跨机 namespace）：`<peer>__<tool>` 的构造与还原。
//!
//! 这是**跨边界契约**，不是某一方的实现细节：
//!
//! - 注册端（大脑侧）把对端工具注册成本机工具名；
//! - 模型侧看到并按这个名字调用；
//! - 远程子代理在自己的工具视图里做同一套名字映射（剥/加前缀）。
//!
//! 三方必须逐字节一致，否则「模型看到 A、注册端认 B」——因此命名规则放在
//! 定义层。
//!
//! ## 命名规范（2026-10 修复）
//!
//! 注册名必须满足 LLM function name 约束 `^[a-zA-Z0-9_-]{1,64}$`
//! （OpenAI / Anthropic 系 provider 均校验）——早期以冒号分隔（`peer:tool`），
//! 严格 provider 会直接 400 拒绝整个请求；现改为**双下划线**分隔
//! （与 MCP 生态的 `server__tool` 惯例一致），peer 名中的非法字符统一
//! 替换为 `_`。

/// 远程工具注册名的 peer 组件清洗：LLM function name 仅允许
/// `[a-zA-Z0-9_-]`，其余字符（含曾用作分隔的冒号）统一替换为 `_`。
pub fn sanitize_peer_component(peer: &str) -> String {
    peer.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// 远程工具对 LLM 暴露的注册名：`<peer>__<tool>`。
pub fn remote_tool_name(peer: &str, tool: &str) -> String {
    format!("{}__{}", sanitize_peer_component(peer), tool)
}

/// 远程工具注册名前缀（远程子代理工具视图的过滤 / 剥前缀用；与
/// [`remote_tool_name`] 同源，保证还原端一致）。
pub fn remote_tool_prefix(peer: &str) -> String {
    format!("{}__", sanitize_peer_component(peer))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_llm_function_name_safe() {
        let name = remote_tool_name("gpu-box", "bash");
        assert_eq!(name, "gpu-box__bash");
        assert!(name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
    }

    #[test]
    fn illegal_peer_chars_are_replaced() {
        // 冒号（2026-10 之前的分隔符）与点、空格都必须清洗
        assert_eq!(sanitize_peer_component("a:b.c d"), "a_b_c_d");
        assert_eq!(remote_tool_name("host:1", "run"), "host_1__run");
    }

    #[test]
    fn prefix_matches_name_construction() {
        // 还原端按 prefix 剥名——两者必须同源
        let prefix = remote_tool_prefix("gpu-box");
        let name = remote_tool_name("gpu-box", "bash");
        assert!(name.starts_with(&prefix));
        assert_eq!(&name[prefix.len()..], "bash");
    }

    #[test]
    fn prefix_uses_the_same_sanitization_as_name() {
        let prefix = remote_tool_prefix("a:b");
        assert_eq!(prefix, "a_b__");
        assert!(remote_tool_name("a:b", "t").starts_with(&prefix));
    }
}

/// 远程工具调用出口（`node://<peer>/…` 前缀的分流目标）。
///
/// 签名 `(peer, tool, args) -> Result<text, error>`：与联邦的 Invoke 一一对应。
/// 定义为 type alias（而非 trait）是刻意的——它只是「一个异步函数」，加一层
/// trait 只会让组合根多写一次装箱；实现方（联邦）本来就是一个闭包。
///
/// ## 为什么在定义层（2026-11 S12）
///
/// 声明方是 `echo-plugin-tools`（`node://` 前缀的**消费方**），实现方是联邦
/// 子系统（组合根装配）。此前它住在 tools 插件里，于是联邦插件不得不
/// `use echo_plugin_tools::RemoteInvoker` —— **两个互不相干的插件因为一个字面量
/// 结了依赖**。类型本身是"一次跨机调用"这个动作的形状，不含任何一方的实现，
/// 所以它的家在定义层。
pub type RemoteInvoker = std::sync::Arc<
    dyn Fn(
            String,
            String,
            serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
        > + Send
        + Sync,
>;
