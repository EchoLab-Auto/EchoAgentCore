//! Multimodal payload hygiene: keep base64 out of the text the model reads.
//!
//! 多模态 API 的正确调用姿势：图片只走独立的 image 内容块（Anthropic
//! `image` block / OpenAI `image_url` part），文本块里只留一个轻量占位符。
//!
//! 历史文本里混入的内嵌 base64 会被端点当**文本** token 计费（实测
//! DeepSeek `/anthropic`：4 万 base64 字符 ≈ 2.8 万输入 token；同一张图走
//! image 块只要约 200 token）——相差两个数量级。1.3MB 截图混在 hook JSON
//! 里就是约 125 万 token，足以打爆 1M 窗口。
//!
//! 两个来源必须处理：
//! 1. `ChatMessage.images` 里逐条携带的 data URI：provider 已把它们作为
//!    image 块下发，文本里再留一份纯属重复计费（本模块把它替换成编号占位）；
//! 2. 文本里残留的其它内嵌 data URI（历史遗留、工具输出回显）：统一省略。

/// 第 `index` 张附图的占位符（`index` 从 1 起，与 image 块顺序一致）。
pub fn image_placeholder(index: usize) -> String {
    format!("[图片#{index}]")
}

/// 行内 base64 图片数据被省略后的占位文本。
pub const INLINE_IMAGE_ELIDED: &str = "[图片数据已省略]";

/// `data:` URI 的 base64 头。
const BASE64_HEADER: &str = ";base64,";

/// 只有达到该长度的内联 base64 串才被省略：短串（测试夹具、图标 data URI）
/// 保持原样，便于阅读与排查。
const MIN_INLINE_B64_CHARS: usize = 256;

/// Whether `ch` may appear in a standard base64 payload.
fn is_base64_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '+' || ch == '/' || ch == '='
}

/// Length (bytes) of the contiguous base64 run at the start of `text`.
///
/// Base64 alphabet characters are all ASCII, so the byte length equals the
/// char count, and the boundary is always a valid char boundary.
fn base64_run_len(text: &str) -> usize {
    text.len() - text.trim_start_matches(is_base64_char).len()
}

/// Collapse embedded image payloads in the model-facing text into placeholders.
///
/// 对每条 `images` 中真正出现在文本里的 data URI，替换为 `[图片#n]`（n 为
/// 该图在本消息 image 块中的序号）；随后省略文本里其余足够长的内联 data URI。
/// 已压过的文本再次调用是幂等的。
pub fn compact_embedded_media(text: &str, images: &[String]) -> String {
    if text.is_empty() {
        return String::new();
    }
    let mut current: Option<String> = None;
    for (index, image) in images.iter().enumerate() {
        if !image.starts_with("data:") {
            continue;
        }
        let base = current.as_deref().unwrap_or(text);
        if !base.contains(image.as_str()) {
            continue;
        }
        current = Some(base.replace(image.as_str(), &image_placeholder(index + 1)));
    }
    let base = current.as_deref().unwrap_or(text);
    elide_inline_data_uris(base)
}

/// Replace every long inline `data:...;base64,<payload>` with a short marker.
///
/// Only payloads of at least [`MIN_INLINE_B64_CHARS`] characters are collapsed,
/// so small fixtures stay legible. The `data:` header itself is preserved for
/// shorter payloads, and the scan always makes progress.
pub fn elide_inline_data_uris(text: &str) -> String {
    const MARKER: &str = "data:";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(position) = rest.find(MARKER) {
        let (head, tail) = rest.split_at(position);
        out.push_str(head);
        let Some(header_position) = tail.find(BASE64_HEADER) else {
            // 后面再没有 base64 头：跳过这个 data: 继续找下一个。
            out.push_str(&tail[..MARKER.len()]);
            rest = &tail[MARKER.len()..];
            continue;
        };
        let body_position = header_position + BASE64_HEADER.len();
        let (prefix, body) = tail.split_at(body_position);
        let run = base64_run_len(body);
        if run >= MIN_INLINE_B64_CHARS {
            out.push_str(INLINE_IMAGE_ELIDED);
        } else {
            out.push_str(prefix);
            out.push_str(&body[..run]);
        }
        rest = &body[run..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_uri(payload: &str) -> String {
        format!("data:image/png;base64,{payload}")
    }

    #[test]
    fn attached_data_uri_becomes_numbered_placeholder() {
        let payload = "A".repeat(400);
        let uri = data_uri(&payload);
        let text = format!("{{\"content\":\"看图\",\"images\":[\"{uri}\"]}}");
        let compacted = compact_embedded_media(&text, std::slice::from_ref(&uri));
        assert!(compacted.contains("[图片#1]"), "{compacted}");
        assert!(!compacted.contains("AAAAAAAA"), "payload removed");
        // 结构其余部分保持不变，仍是可读 JSON。
        assert!(compacted.contains("\"content\":\"看图\""));
    }

    #[test]
    fn second_image_gets_second_placeholder_in_order() {
        let one = data_uri(&"A".repeat(300));
        let two = data_uri(&"B".repeat(300));
        let text = format!("{{\"images\":[\"{one}\",\"{two}\"]}}");
        let compacted = compact_embedded_media(&text, &[one, two]);
        assert!(compacted.contains("[图片#1]") && compacted.contains("[图片#2]"));
        assert!(compacted.find("[图片#1]") < compacted.find("[图片#2]"));
    }

    #[test]
    fn stale_inline_data_uri_without_attachment_is_elided() {
        let text = format!("log line {}", data_uri(&"C".repeat(2048)));
        let compacted = compact_embedded_media(&text, &[]);
        assert!(compacted.contains(INLINE_IMAGE_ELIDED), "{compacted}");
        assert!(!compacted.contains("CCCC"));
    }

    #[test]
    fn short_payloads_stay_readable() {
        let text = "fixture data:image/png;base64,QUJD end";
        assert_eq!(compact_embedded_media(text, &[]), text);
    }

    #[test]
    fn compaction_is_idempotent() {
        let uri = data_uri(&"D".repeat(500));
        let text = format!("x {uri} y");
        let once = compact_embedded_media(&text, std::slice::from_ref(&uri));
        let twice = compact_embedded_media(&once, &[uri]);
        assert_eq!(once, twice);
    }

    #[test]
    fn dangling_data_header_does_not_loop_or_lose_text() {
        let text = "a data: no base64 here, and more text";
        assert_eq!(elide_inline_data_uris(text), text);
        let mixed = "data:image/png;base64,SHORT and data:image/png;base64,QUJD";
        assert_eq!(elide_inline_data_uris(mixed), mixed);
    }

    #[test]
    fn remote_urls_are_not_touched() {
        let text = "see https://example.com/a.png and data:image/jpeg;base64,QUJD";
        assert_eq!(elide_inline_data_uris(text), text);
    }
}
