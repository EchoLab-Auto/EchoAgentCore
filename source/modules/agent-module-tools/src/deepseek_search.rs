//! DeepSeek 官方搜索 / 读页 API 客户端 + `read_url` 工具。
//!
//! # 为什么不用 Web Tools（server tools）
//!
//! DeepSeek 另有 `deepseek_web_search` / `deepseek_read_url` 这类**服务端工具**
//! （在请求 `tools` 里声明、由 DeepSeek 服务端执行）。那条路的代价是：
//! 只对 deepseek 通道有效、要把 `ToolDefinition` 扩出协议 `type` 字段、两个
//! provider 的序列化各改一遍、响应里 `server_tool_use` / `web_search_tool_result`
//! 块还要额外解析（否则面板看不到搜索过程）。
//!
//! 这里用的是同一家公司的 **Search & Fetch 原始 JSON API**（文档：
//! `api-docs.deepseek.com/guides/search.md`），形态就是普通 HTTP：
//!
//! - `POST https://api.deepseek.com/v1/tools/search` → `{results: [...]}`
//! - `POST https://api.deepseek.com/v1/tools/fetch`  → `{title, content}`
//!
//! 于是它就是一个**普通 `Tool`**：零协议改动、任意 provider 都能用、结果格式
//! 我们完全掌控，而且返回的 `summary`（600–800 字，官方标注"intended to be
//! read by a model"）比 Bing RSS 的 100–200 字 snippet 质量高一个档次。
//!
//! # 密钥从哪来
//!
//! 工具的构造期拿不到 `AgentConfig`（注册发生在模块 mount），所以密钥在**调用
//! 期**从共享 `ConfigStore` 现读：先看 `[agent]` 顶层（`provider = "deepseek"`
//! 且 `api_key` 非空），再扫 `[[agent.api_profiles]]` 里第一个 provider 为
//! `deepseek` 且密钥非空的条目。这样"运行期换 profile"下一 turn 就生效，不需要
//! 重建工具。
//!
//! # 与 Bing 的关系
//!
//! 本模块**不替换** [`crate::websearch`]：引擎由 `[agent] web_search_engine`
//! 选择（`"bing"` 默认 / `"deepseek"`），且 DeepSeek 路径失败时 `web_search`
//! 会回落 Bing——线上搜索能力不因为多了一条路而变脆。

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{Tool, ToolError};

/// DeepSeek API 根（Search / Fetch 都挂在它下面）。
const API_BASE: &str = "https://api.deepseek.com";
/// 搜索端点。
const SEARCH_PATH: &str = "/v1/tools/search";
/// 读页端点。
const FETCH_PATH: &str = "/v1/tools/fetch";
/// 单次调用超时。搜索/读页是同步抓取，给得比普通工具宽一点。
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// 返回给模型的单条 `summary` 上限：官方典型 600–800 字，按字符截断防爆上下文。
const SUMMARY_CLIP_CHARS: usize = 1200;
/// 返回给模型的单条 `content` 上限（仅在 `need_content` 时出现）。
const CONTENT_CLIP_CHARS: usize = 6000;
/// `read_url` 正文上限：整页 Markdown 可能几万字，直接塞进上下文会挤掉历史。
const FETCH_CLIP_CHARS: usize = 20000;

/// 搜索引擎选择（`[agent] web_search_engine`）。
///
/// 解析宽松：大小写不敏感，未知值按默认 [`SearchEngine::Bing`] 处理——配置写错
/// 不该让搜索能力整体消失。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchEngine {
    /// Bing RSS（无需密钥，默认）。
    #[default]
    Bing,
    /// DeepSeek 官方 Search API（需 deepseek 密钥）。
    DeepSeek,
}

impl SearchEngine {
    /// 从配置字符串解析；空 / 未知 → Bing。
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "deepseek" | "ds" => Self::DeepSeek,
            _ => Self::Bing,
        }
    }
}

/// 一条搜索结果（只留对模型有用的字段，`rerank_score` 已用于排序）。
#[derive(Debug, Clone, Default)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub summary: String,
    pub content: String,
    pub site_name: String,
    pub date: String,
}

/// 一次读页的结果。
#[derive(Debug, Clone, Default)]
pub struct FetchedPage {
    pub title: String,
    pub content: String,
}

/// DeepSeek 搜索 / 读页客户端。
///
/// `store` 只用于在调用期解析密钥（注册期没有配置）；`client` 与其它工具共享
/// 同一份连接池。
pub struct DeepSeekClient {
    store: echo_config::ConfigStore,
    client: Arc<reqwest::Client>,
}

/// 手写 Debug：`ConfigStore` 不含 `Debug`（它持有路径与锁，没有可读的投影）。
/// 只打路径——足够定位"是哪个配置在生效"，且不会泄露密钥。
impl std::fmt::Debug for DeepSeekClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeepSeekClient")
            .field("config", &self.store.path())
            .finish()
    }
}

impl DeepSeekClient {
    pub fn new(store: echo_config::ConfigStore, client: Arc<reqwest::Client>) -> Self {
        Self { store, client }
    }

    /// 当前配置下可用的 deepseek 密钥（无则 `None`）。
    pub fn api_key(&self) -> Option<String> {
        if !self.store.path().exists() {
            return None;
        }
        let document = self.store.read().ok()?;
        resolve_api_key(&document)
    }

    /// 是否可用（决定 `web_search` 走哪条引擎）。
    pub fn available(&self) -> bool {
        self.api_key().is_some()
    }

    /// `POST /v1/tools/search`。结果按 `rerank_score` 降序（上游已排好，这里再
    /// 兜一次底，避免上游改行为后我们拿到乱序结果）。
    pub async fn search(
        &self,
        query: &str,
        count: usize,
        need_content: bool,
    ) -> Result<Vec<SearchHit>, String> {
        let key = self.api_key().ok_or_else(|| {
            "没有可用的 DeepSeek 密钥（[agent].api_key 或某个 deepseek profile）".to_string()
        })?;
        let body = json!({
            "query": query,
            "count": count.clamp(1, 50),
            "need_content": need_content,
        });
        let text = self.post(SEARCH_PATH, &key, &body).await?;
        parse_search_response(&text)
    }

    /// `POST /v1/tools/fetch`。
    pub async fn fetch(&self, url: &str) -> Result<FetchedPage, String> {
        let key = self.api_key().ok_or_else(|| {
            "没有可用的 DeepSeek 密钥（[agent].api_key 或某个 deepseek profile）".to_string()
        })?;
        let body = json!({ "url": url });
        let text = self.post(FETCH_PATH, &key, &body).await?;
        parse_fetch_response(&text)
    }

    /// 两个端点共用的 POST（超时 / 状态码 / 错误体翻译）。
    async fn post(&self, path: &str, key: &str, body: &Value) -> Result<String, String> {
        let url = format!("{API_BASE}{path}");
        let request = self.client.post(&url).bearer_auth(key).json(body).send();
        let response = tokio::time::timeout(REQUEST_TIMEOUT, request)
            .await
            .map_err(|_| format!("请求超时（>{}s）", REQUEST_TIMEOUT.as_secs()))?
            .map_err(|e| format!("请求失败: {e}"))?;
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|e| format!("读取响应失败: {e}"))?;
        if !status.is_success() {
            return Err(format!(
                "HTTP {status}: {}",
                explain_error(status.as_u16(), &text)
            ));
        }
        Ok(text)
    }

    /// 当前配置选择的搜索引擎（`[agent] web_search_engine`，缺省 Bing）。
    ///
    ///
    /// 调用期现读而不是构造期固化：面板改完配置**下一 turn 生效**，不需要重启
    /// Core 重建工具。读失败按默认 Bing 处理——配置坏了不该让搜索消失。
    pub fn engine(&self) -> SearchEngine {
        if !self.store.path().exists() {
            return SearchEngine::default();
        }
        let Ok(document) = self.store.read() else {
            return SearchEngine::default();
        };
        let raw = document
            .get("agent")
            .and_then(|agent| agent.get("web_search_engine"))
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        SearchEngine::parse(raw)
    }
}

/// 把上游错误体翻译成可执行的说法（照搬文档的错误码表里最常见的几种）。
fn explain_error(status: u16, body: &str) -> String {
    let detail = error_message(body);
    let hint = match status {
        401 => "API Key 无效或缺失",
        402 => "账户余额不足",
        403 => "目标站点被安全策略拒绝（仅 Fetch）",
        404 => "抓不到可用正文（URL 不可达 / 站点策略 / 无可提取内容）",
        429 => "触发了 Search/Fetch 的 QPS 限流",
        _ => "",
    };
    if hint.is_empty() {
        detail
    } else {
        format!("{hint} — {detail}")
    }
}

/// 取 `{"error":{"message":...}}` 里的 message（拿不到就截断原文）。
fn error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| clip(body, 300))
}

/// 解析 `/v1/tools/search` 的响应。
fn parse_search_response(text: &str) -> Result<Vec<SearchHit>, String> {
    let root: Value = serde_json::from_str(text)
        .map_err(|e| format!("响应解析失败: {e} — {}", clip(text, 300)))?;
    let Some(items) = root.get("results").and_then(|v| v.as_array()) else {
        return Ok(Vec::new());
    };
    let mut hits: Vec<(f64, SearchHit)> = items
        .iter()
        .map(|item| {
            let score = item
                .get("rerank_score")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let hit = SearchHit {
                title: str_field(item, "title"),
                url: str_field(item, "url"),
                snippet: str_field(item, "snippet"),
                summary: clip(&str_field(item, "summary"), SUMMARY_CLIP_CHARS),
                content: clip(&str_field(item, "content"), CONTENT_CLIP_CHARS),
                site_name: str_field(item, "site_name"),
                date: str_field(item, "date"),
            };
            (score, hit)
        })
        .collect();
    // 稳定排序：同分保持上游顺序。
    hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    Ok(hits.into_iter().map(|(_, hit)| hit).collect())
}

/// 解析 `/v1/tools/fetch` 的响应。
fn parse_fetch_response(text: &str) -> Result<FetchedPage, String> {
    let root: Value = serde_json::from_str(text)
        .map_err(|e| format!("响应解析失败: {e} — {}", clip(text, 300)))?;
    Ok(FetchedPage {
        title: str_field(&root, "title"),
        content: clip(&str_field(&root, "content"), FETCH_CLIP_CHARS),
    })
}

/// 从配置文档里挑 deepseek 密钥：先顶层，再供应商池。
///
/// 顶层优先的原因：`active_api` 为空时顶层就是生效配置；有 profile 时顶层
/// `api_key` 通常为空（各 persona 各自指定），于是自然落到池里那一条。
fn resolve_api_key(document: &toml::Table) -> Option<String> {
    let agent = document.get("agent")?.as_table()?;
    let provider = agent
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let key = agent
        .get("api_key")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .trim();
    if provider == "deepseek" && !key.is_empty() {
        return Some(key.to_string());
    }
    let profiles = agent.get("api_profiles").and_then(|v| v.as_array())?;
    for profile in profiles {
        let Some(profile) = profile.as_table() else {
            continue;
        };
        let provider = profile
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let key = profile
            .get("api_key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .trim();
        if provider == "deepseek" && !key.is_empty() {
            return Some(key.to_string());
        }
    }
    None
}

fn str_field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string()
}

/// 按字符（不是字节）截断，避免切断多字节字符。
fn clip(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push('…');
    out
}

/// 把搜索结果渲染成模型可读文本。
pub fn render_hits(query: &str, hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return format!("未搜索到关于“{query}”的结果。");
    }
    let mut out = Vec::with_capacity(hits.len());
    for (index, hit) in hits.iter().enumerate() {
        let mut block = format!("{}. {}", index + 1, hit.title);
        if !hit.url.is_empty() {
            block.push('\n');
            block.push_str(&hit.url);
        }
        // meta 行：站点 / 发布时间（两者都可能缺，全缺就不占行）。
        let mut meta = Vec::new();
        if !hit.site_name.is_empty() {
            meta.push(hit.site_name.clone());
        }
        if !hit.date.is_empty() {
            meta.push(hit.date.clone());
        }
        if !meta.is_empty() {
            block.push('\n');
            block.push_str(&meta.join(" · "));
        }
        // 正文优先于 summary 优先于 snippet：越靠后质量越低。
        let body = if !hit.summary.is_empty() {
            &hit.summary
        } else if !hit.snippet.is_empty() {
            &hit.snippet
        } else {
            &hit.content
        };
        if !body.is_empty() {
            block.push('\n');
            block.push_str(body);
        }
        // `need_content` 命中且有额外正文时附在后面（摘要已经讲过的不重复）。
        if !hit.content.is_empty() && !hit.summary.is_empty() {
            block.push_str("\n正文节选：\n");
            block.push_str(&hit.content);
        }
        out.push(block);
    }
    out.join("\n\n")
}

// ---------------------------------------------------------------------------
// read_url 工具
// ---------------------------------------------------------------------------

/// 读取一个网页的正文（DeepSeek Fetch API，返回 Markdown）。
///
/// 定位：`web_search` 给的是摘要，需要看细节（安装步骤、API 表格、报错原文）时
/// 用它。与 `web_search` 共用同一个 [`DeepSeekClient`]（同一把密钥、同一个连接池）。
#[derive(Debug)]
pub struct ReadUrlTool {
    deepseek: Arc<DeepSeekClient>,
}

impl ReadUrlTool {
    pub fn new(deepseek: Arc<DeepSeekClient>) -> Self {
        Self { deepseek }
    }
}

#[async_trait]
impl Tool for ReadUrlTool {
    fn name(&self) -> &str {
        "read_url"
    }

    fn description(&self) -> &str {
        "读取一个网页的正文（Markdown）。当 web_search 的摘要不够、需要看原文细节\
         （文档、安装步骤、报错信息、API 表格）时使用。参数: url(完整 http/https 地址)。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "要读取的页面地址（http:// 或 https://）"}
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        let url = arguments
            .get("url")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("缺少 url 参数".into()))?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(ToolError::InvalidArguments(
                "url 必须是 http:// 或 https:// 开头的完整地址".into(),
            ));
        }
        let page = self
            .deepseek
            .fetch(url)
            .await
            .map_err(ToolError::Execution)?;
        let title = if page.title.is_empty() {
            url.to_string()
        } else {
            page.title
        };
        Ok(format!("{title}\n\n{}", page.content))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEARCH_SAMPLE: &str = r#"{
      "results": [
        {
          "rerank_score": 1.2,
          "title": "低分结果",
          "url": "https://low.example.com",
          "snippet": "低分摘要",
          "summary": "低分长摘要",
          "content": "",
          "site_name": null,
          "date": null
        },
        {
          "rerank_score": 2.4,
          "title": "DeepSeek V4 model released",
          "url": "https://example.com/deepseek-v4",
          "snippet": "DeepSeek V4 is now available.",
          "summary": "DeepSeek has released the V4 model.",
          "content": "DeepSeek V4 is now generally available...",
          "site_name": "Example News",
          "date": "2026-06-01T00:00:00+00:00"
        }
      ]
    }"#;

    #[test]
    fn engine_parsing_is_lenient() {
        assert_eq!(SearchEngine::parse("deepseek"), SearchEngine::DeepSeek);
        assert_eq!(SearchEngine::parse("  DeepSeek "), SearchEngine::DeepSeek);
        assert_eq!(SearchEngine::parse("bing"), SearchEngine::Bing);
        // 未知/空值一律回落 Bing：配置写错不该让搜索整体失效。
        assert_eq!(SearchEngine::parse(""), SearchEngine::Bing);
        assert_eq!(SearchEngine::parse("google"), SearchEngine::Bing);
    }

    #[test]
    fn search_results_sorted_by_rerank_score() {
        let hits = parse_search_response(SEARCH_SAMPLE).expect("sample parses");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "DeepSeek V4 model released", "高分在前");
        assert_eq!(hits[0].site_name, "Example News");
        assert_eq!(hits[0].date, "2026-06-01T00:00:00+00:00");
        // null 字段退化成空串，不 panic。
        assert_eq!(hits[1].site_name, "");
        assert_eq!(hits[1].date, "");
    }

    #[test]
    fn empty_results_array_is_not_an_error() {
        let hits = parse_search_response(r#"{"results": []}"#).unwrap();
        assert!(hits.is_empty());
        // 缺 results 字段同样按空处理（上游裁剪字段时不炸）。
        assert!(parse_search_response("{}").unwrap().is_empty());
    }

    #[test]
    fn malformed_body_reports_parse_error_with_excerpt() {
        let error = parse_search_response("not json").unwrap_err();
        assert!(error.contains("解析失败"), "{error}");
        assert!(error.contains("not json"), "{error}");
    }

    #[test]
    fn render_prefers_summary_and_appends_content() {
        let hits = parse_search_response(SEARCH_SAMPLE).unwrap();
        let text = render_hits("DeepSeek V4", &hits[..1]);
        assert!(text.contains("1. DeepSeek V4 model released"), "{text}");
        assert!(text.contains("https://example.com/deepseek-v4"), "{text}");
        assert!(text.contains("Example News · 2026-06-01"), "{text}");
        // summary 优先于 snippet/content。
        assert!(
            text.contains("DeepSeek has released the V4 model."),
            "{text}"
        );
        // 有 summary 时 content 以「正文节选」追加。
        assert!(text.contains("正文节选"), "{text}");
    }

    #[test]
    fn render_without_hits_explains_the_query() {
        let text = render_hits("无结果查询", &[]);
        assert!(text.contains("无结果查询"), "{text}");
        assert!(text.contains("未搜索到"), "{text}");
    }

    #[test]
    fn fetch_response_parses_title_and_content() {
        // 正文是 Markdown（以 `# ` 开头）：用 json! 构造，避免原始字符串
        // 被 `"#` 提前截断。
        let body =
            json!({"title": "Example article", "content": "# Example article\n\nbody"}).to_string();
        let page = parse_fetch_response(&body).unwrap();
        assert_eq!(page.title, "Example article");
        assert!(page.content.starts_with("# Example article"));
    }

    #[test]
    fn long_content_is_clipped_by_chars() {
        let long = "测".repeat(FETCH_CLIP_CHARS + 100);
        let page = parse_fetch_response(&json!({"title":"t","content":long}).to_string()).unwrap();
        assert_eq!(
            page.content.chars().count(),
            FETCH_CLIP_CHARS + 1,
            "末尾一个省略号"
        );
        assert!(page.content.ends_with('…'));
    }

    /// 密钥解析：顶层（provider=deepseek 且非空）优先；否则落到池里第一个
    /// deepseek profile。非 deepseek 的顶层密钥**绝不**被当作搜索密钥用。
    #[test]
    fn api_key_prefers_top_level_then_pool() {
        let top = r#"
            [agent]
            provider = "deepseek"
            api_key = "sk-top"
        "#
        .parse::<toml::Table>()
        .unwrap();
        assert_eq!(resolve_api_key(&top).as_deref(), Some("sk-top"));

        // 顶层是 openai：即使有密钥也不能拿来打 deepseek 端点。
        let other = r#"
            [agent]
            provider = "openai"
            api_key = "sk-openai"
            [[agent.api_profiles]]
            name = "ds"
            provider = "deepseek"
            api_key = "sk-pool"
        "#
        .parse::<toml::Table>()
        .unwrap();
        assert_eq!(resolve_api_key(&other).as_deref(), Some("sk-pool"));

        // 一个都没有 → None（搜索回落 Bing）。
        let none = r#"
            [agent]
            provider = "openai"
            api_key = "sk-openai"
        "#
        .parse::<toml::Table>()
        .unwrap();
        assert_eq!(resolve_api_key(&none), None);
    }

    #[test]
    fn upstream_error_message_is_surfaced() {
        let body = r#"{"error":{"message":"'count' must be between 1 and 50","type":"invalid_request_error","param":null,"code":"invalid_request_error"}}"#;
        let text = explain_error(400, body);
        assert!(text.contains("count"), "{text}");
        // 已知状态码补一句"下一步该做什么"。
        let limited = explain_error(429, body);
        assert!(limited.contains("QPS"), "{limited}");
    }
}
