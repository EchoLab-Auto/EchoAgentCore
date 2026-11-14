//! 端到端重现：用**磁盘上的真令牌**、真实的 `ChatGptProvider` 发一次请求。
//!
//! 前面两版 probe 已排除网络与代理（0/8/64/512KB × h2/http1 全 401），
//! 所以问题只可能在 provider 内部状态。这里走的就是生产那条路径：
//! `auth::load()` → `ChatGptProvider::new()` → `chat()`。
fn main() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(run());
}

async fn run() {
    println!("ECHO_CHATGPT_PROXY = {:?}", std::env::var("ECHO_CHATGPT_PROXY").ok());
    let auth = match agent_module_provider_llm::chatgpt::auth::load() {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("没有登录态");
            return;
        }
        Err(e) => {
            println!("读令牌失败: {e}");
            return;
        }
    };
    println!("email = {} expired = {}", auth.email, auth.is_expired());

    use echo_defs::{ChatMessage, ChatRequest, LlmProvider};
    let provider = match agent_module_provider_llm::chatgpt::ChatGptProvider::new(auth, &std::env::var("PROBE_MODEL").unwrap_or_else(|_| "gpt-5".into())) {
        Ok(provider) => provider,
        Err(error) => {
            println!("建 provider 失败（这一步就该报出来）：{error}");
            return;
        }
    };
    let model = std::env::var("PROBE_MODEL").unwrap_or_else(|_| "gpt-5".into());
    let req = ChatRequest {
        model,
        messages: vec![ChatMessage::user("说\"ok\"两个字")],
        tools: None,
        temperature: None,
        max_tokens: Some(64),
    };
    let started = std::time::Instant::now();
    match provider.chat(&req).await {
        Ok(r) => println!("{}ms 成功: {:?}", started.elapsed().as_millis(), r.content),
        Err(e) => {
            println!("{}ms 失败: {e}", started.elapsed().as_millis());
            let mut src: &dyn std::error::Error = &e;
            while let Some(next) = src.source() {
                println!("    └─ {next}");
                src = next;
            }
        }
    }
}
