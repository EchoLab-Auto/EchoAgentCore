//! Web search tool — Bing RSS-backed search, no API key required.
//!
//! Uses the public `cn.bing.com/search?format=rss` endpoint (reachable from
//! mainland networks) and parses the returned RSS items.

use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

use crate::tool::{Tool, ToolError};

#[derive(Debug, Default)]
pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "搜索网页获取最新信息（新闻、天气、百科、实时数据）。参数: query(搜索关键词), max_results(可选, 默认5, 最大10)。"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "搜索关键词"},
                "max_results": {"type": "integer", "description": "返回结果数量，默认5，最大10"}
            },
            "required": ["query"]
        })
    }

    async fn execute(&self, arguments: Value) -> Result<String, ToolError> {
        let query = arguments
            .get("query")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("缺少 query 参数".into()))?;

        let max_results = arguments
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 10) as usize;

        let results = search_bing(query, max_results).await?;
        // Filter junk (Bing navigation pages, duplicates, irrelevant entries),
        // then cap to the requested count.
        let mut results = filter_results(query, results);
        results.truncate(max_results);

        if results.is_empty() {
            return Ok(format!("未搜索到关于“{query}”的结果。"));
        }

        let mut out = Vec::new();
        for (i, r) in results.iter().enumerate() {
            let mut block = format!("{}. {}", i + 1, r.title);
            if !r.url.is_empty() {
                block.push('\n');
                block.push_str(&r.url);
            }
            if !r.snippet.is_empty() {
                block.push('\n');
                block.push_str(&r.snippet);
            }
            out.push(block);
        }
        Ok(out.join("\n\n"))
    }
}

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn client() -> &'static reqwest::Client {
    // 进程级单例存储收敛于 kernel cell（唯一引导单元）；初值只构造一次
    // （Box::leak 一次，生命周期与旧的进程级存储一致）。
    echo_context::kernel::get_or_init(|| {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36")
            .build()
            .expect("failed to build reqwest client for web_search");
        let leaked: &'static reqwest::Client = Box::leak(Box::new(client));
        leaked
    })
}

async fn search_bing(query: &str, max_results: usize) -> Result<Vec<SearchResult>, ToolError> {
    let resp = client()
        .get("https://cn.bing.com/search")
        .query(&[("q", query), ("format", "rss")])
        .send()
        .await
        .map_err(|e| ToolError::Execution(format!("搜索请求失败: {e}")))?;

    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| ToolError::Execution(format!("读取搜索响应失败: {e}")))?;

    if !status.is_success() {
        return Err(ToolError::Execution(format!("搜索服务返回状态码 {status}")));
    }

    Ok(parse_rss(&body, max_results))
}

fn parse_rss(body: &str, max_results: usize) -> Vec<SearchResult> {
    let item_re = Regex::new(r"(?s)<item>(.*?)</item>").expect("item regex");
    let title_re = Regex::new(r"(?s)<title>(.*?)</title>").expect("title regex");
    let link_re = Regex::new(r"(?s)<link>(.*?)</link>").expect("link regex");
    let desc_re = Regex::new(r"(?s)<description>(.*?)</description>").expect("desc regex");

    let mut results = Vec::new();
    for cap in item_re.captures_iter(body).take(max_results * 2) {
        let block = &cap[1];
        let title = extract(block, &title_re);
        let url = extract(block, &link_re);
        let snippet = extract(block, &desc_re);
        if title.is_empty() && url.is_empty() {
            continue;
        }
        results.push(SearchResult {
            title: decode_entities(&clean_html(&title)),
            url: decode_entities(&url),
            snippet: decode_entities(&clean_html(&snippet)),
        });
        if results.len() >= max_results * 2 {
            break;
        }
    }
    results
}

/// Filter out junk results: Bing's own navigation pages, duplicates, and
/// entries unrelated to the query (e.g. dictionary pop-ups that share no
/// keyword with the search). The raw parser may over-collect, so filtering
/// happens before the caller renders the final list.
fn filter_results(query: &str, results: Vec<SearchResult>) -> Vec<SearchResult> {
    let keywords = extract_keywords(query);
    let mut seen = std::collections::HashSet::new();
    results
        .into_iter()
        .filter(|result| {
            // Skip Bing-internal navigation/feature pages.
            let host = result
                .url
                .trim_start_matches("https://")
                .trim_start_matches("http://")
                .split('/')
                .next()
                .unwrap_or_default()
                .to_lowercase();
            if matches!(
                host.as_str(),
                "bing.com" | "www.bing.com" | "cn.bing.com" | "global.bing.com"
            ) {
                return false;
            }
            // Skip duplicate titles.
            let key = result.title.to_lowercase();
            if !seen.insert(key) {
                return false;
            }
            // Keep results that share at least one keyword with the query.
            if keywords.is_empty() {
                return true;
            }
            let haystack = format!(
                "{} {}",
                result.title.to_lowercase(),
                result.snippet.to_lowercase()
            );
            keywords.iter().any(|keyword| haystack.contains(keyword))
        })
        .collect()
}

/// Extract searchable keywords from a query: CJK runs and ASCII words with at
/// least 2 characters. Single CJK chars (stopword-like) are dropped.
fn extract_keywords(query: &str) -> Vec<String> {
    let mut keywords = Vec::new();
    let mut cjk_run = String::new();
    let push_cjk = |run: &mut String, keywords: &mut Vec<String>| {
        if run.chars().count() >= 2 {
            keywords.push(run.clone());
        }
        run.clear();
    };
    let mut word = String::new();
    for ch in query.chars() {
        if ch.is_ascii_alphanumeric() {
            word.push(ch);
        } else {
            if word.len() >= 2 {
                keywords.push(word.to_lowercase());
            }
            word.clear();
            if is_cjk_char(ch) {
                cjk_run.push(ch);
            } else {
                push_cjk(&mut cjk_run, &mut keywords);
            }
            continue;
        }
        push_cjk(&mut cjk_run, &mut keywords);
    }
    if word.len() >= 2 {
        keywords.push(word.to_lowercase());
    }
    push_cjk(&mut cjk_run, &mut keywords);
    keywords
}

fn is_cjk_char(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x303F
        | 0x3040..=0x30FF
        | 0x31C0..=0x31EF
        | 0x3400..=0x9FFF
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFFEF
        | 0x20000..=0x2FA1F
    )
}

fn extract(block: &str, re: &Regex) -> String {
    re.captures(block)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .unwrap_or_default()
}

/// Remove HTML tags and collapse whitespace.
fn clean_html(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    let tag_re = Regex::new(r"<[^>]*>").expect("tag regex");
    let ws_re = Regex::new(r"\s+").expect("ws regex");
    ws_re
        .replace_all(&tag_re.replace_all(s, " "), " ")
        .trim()
        .to_string()
}

/// Decode common XML entities.
fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="utf-8" ?><rss version="2.0"><channel>
<title>必应：测试</title>
<item><title>测试标题 &amp; 更多</title><link>https://example.com/a</link><description>这是<b>摘要</b>内容，&amp; 带实体。</description></item>
<item><title>第二条</title><link>https://example.com/b</link><description></description></item>
<item><title></title><link></link><description>无标题无链接应被跳过</description></item>
</channel></rss>"#;

    #[test]
    fn parses_rss_items() {
        let results = parse_rss(SAMPLE, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "测试标题 & 更多");
        assert_eq!(results[0].url, "https://example.com/a");
        assert_eq!(results[0].snippet, "这是 摘要 内容，& 带实体。");
        assert_eq!(results[1].title, "第二条");
        assert_eq!(results[1].snippet, "");
    }

    #[test]
    fn parse_rss_over_collects_candidates_for_filtering() {
        // parse_rss now gathers up to 2x the requested count so junk can be
        // filtered before truncation; the final count is capped by execute().
        let results = parse_rss(SAMPLE, 1);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "测试标题 & 更多");
        let filtered = filter_results("测试", results);
        // 第二条与查询词无关，被相关性过滤丢弃。
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].title, "测试标题 & 更多");
    }

    #[test]
    fn decodes_entities() {
        assert_eq!(
            decode_entities("a&amp;b&lt;c&gt;d&quot;e&#39;f"),
            "a&b<c>d\"e'f"
        );
    }

    #[test]
    fn filters_out_bing_navigation_and_irrelevant_entries() {
        let results = vec![
            SearchResult {
                title: "异环 - 百度百科".into(),
                url: "https://baike.baidu.com/item/异环".into(),
                snippet: "异环是游戏《异环》中的虚构地点。".into(),
            },
            SearchResult {
                title: "字典：异".into(),
                url: "https://cn.bing.com/search?q=异".into(),
                snippet: "异：不同的、特别的。".into(),
            },
            SearchResult {
                title: "兑换码领取攻略".into(),
                url: "https://example.com/codes".into(),
                snippet: "异环1.3版本兑换码大全。".into(),
            },
            SearchResult {
                title: "某无关新闻".into(),
                url: "https://news.example.com/x".into(),
                snippet: "今日天气晴朗。".into(),
            },
        ];
        let filtered = filter_results("异环 兑换码", results);
        // Bing 导航页被过滤；字典条目（不含查询词）被过滤；无关新闻被过滤。
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().all(|r| r.title != "字典：异"));
        assert!(filtered.iter().all(|r| !r.url.contains("bing.com/search")));
        assert!(filtered.iter().any(|r| r.title == "异环 - 百度百科"));
        assert!(filtered.iter().any(|r| r.title == "兑换码领取攻略"));
    }

    #[test]
    fn duplicate_titles_are_deduplicated() {
        let results = vec![
            SearchResult {
                title: "同一条新闻".into(),
                url: "https://a.example.com/1".into(),
                snippet: "异环内容".into(),
            },
            SearchResult {
                title: "同一条新闻".into(),
                url: "https://b.example.com/2".into(),
                snippet: "异环内容".into(),
            },
        ];
        let filtered = filter_results("异环", results);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn empty_query_keywords_keep_all_results() {
        let results = vec![SearchResult {
            title: "anything".into(),
            url: "https://x.example.com".into(),
            snippet: "".into(),
        }];
        assert_eq!(filter_results("!!", results).len(), 1);
    }

    #[test]
    fn extract_keywords_splits_cjk_and_ascii() {
        assert_eq!(extract_keywords("异环 兑换码"), vec!["异环", "兑换码"]);
        assert_eq!(extract_keywords("deepseek r1"), vec!["deepseek", "r1"]);
        // Single CJK chars are dropped as stopword-like.
        assert_eq!(extract_keywords("的"), Vec::<String>::new());
        assert_eq!(extract_keywords("异环1.3版本"), vec!["异环", "版本"]);
    }

    #[tokio::test]
    #[ignore = "需要外网，按需手动运行"]
    async fn live_bing_search() {
        let results = search_bing("沪深指数", 3).await.unwrap();
        assert!(!results.is_empty(), "live search should return results");
        println!("=== live search results ===");
        for r in &results {
            println!("TITLE: {}", r.title);
            println!("URL:   {}", r.url);
            println!("SNIP:  {}", r.snippet);
            println!("---");
        }
    }
}
