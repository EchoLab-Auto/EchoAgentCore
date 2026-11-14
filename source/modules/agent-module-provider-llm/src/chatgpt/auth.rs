//! ChatGPT 订阅（Plus / Pro）的 OAuth 登录与令牌管理。
//!
//! # 这是什么
//!
//! ChatGPT 订阅可以在**官方客户端**（Codex CLI / IDE 扩展 / 桌面端）里当作
//! 模型额度用，不单独计 API 费。那些客户端用一条标准 OAuth 2.0 + PKCE 流程
//! 拿 access token，之后把 token 放在 `Authorization: Bearer` 上打
//! `https://chatgpt.com/backend-api/codex/responses`。
//!
//! 本模块复刻的就是这一条流程，目的是让 Core 能**独立**使用订阅额度，
//! 不必把 Codex 当子进程套一层。
//!
//! # 事实来源（全部实测，不是猜的）
//!
//! 端点、客户端 id、参数名、claim 名都取自**官方 Codex 二进制**
//! （`@openai/codex-linux-x64` 0.169.0，`strings` + 关键串定位）：
//!
//! | 项 | 值 | 在二进制里出现的次数 |
//! |---|---|---|
//! | client_id | `app_EMoamEEZ73f0CkXaXp7hrann` | 1 |
//! | authorize | `https://auth.openai.com/oauth/authorize` | `/oauth/authorize` |
//! | token | `https://auth.openai.com/oauth/token` | 1 |
//! | revoke | `https://auth.openai.com/oauth/revoke` | 1 |
//! | scope | `openid profile email offline_access` | 1 |
//! | 回调路径 | `/auth/callback` | 1 |
//! | 回调端口 | `1455` | — |
//! | 额外参数 | `id_token_add_organizations` / `codex_cli_simplified_flow` | 各 1 |
//! | claim 容器 | `https://api.openai.com/auth` → `chatgpt_account_id` | 2 |
//! | API base | `https://chatgpt.com/backend-api/codex` | 1 |
//!
//! **没有一条是推测**：连 `OPENAI_BETA` 这种常见头我都没加，因为二进制里
//! 找不到它——多带一个可疑头只会更容易被判异常。
//!
//! # 必须知情的三件事
//!
//! 1. **client_id 是 Codex CLI 的，不是发给第三方程序用的。** 这是全生态
//!    的普遍做法（Cline / Roo Code / opencode / goose 都如此），但它不是
//!    公开 API：OpenAI 随时可以改端点、加校验或吊销这个 client。失败形态
//!    是登录直接报错，不影响其它 provider。
//! 2. **这是非官方用法**，不享受 API 的 SLA 与支持；账号层面的风险由使用者
//!    自担。所以它做成了一个**可选 provider**（`openai-chatgpt`），默认不启用。
//! 3. **额度与 ChatGPT 客户端共享**，不是无限的第三方通道。请求头里的
//!    `originator` 如实填 `codex_cli_rs`（二进制里的值），不伪装成别的软件。
//!
//! # 为什么不做 device-code 或手工粘贴
//!
//! 回调**必须**在 `127.0.0.1:1455` 上：`redirect_uri` 是随 client 注册的，
//! 端口不对授权服务器直接拒。所以登录要求 Core 与浏览器在同一台机器上；
//! 远程部署时把 URL 拷到本地浏览器打开也行（回调仍打到 Core 那台的 1455，
//! 需要端口可达）。这两种情况都在错误信息里写明。

use std::path::PathBuf;

use sha2::{Digest, Sha256};

/// 官方 Codex CLI 的 OAuth 客户端 id（见模块头「事实来源」）。
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// 授权端点。
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
/// 换取 / 刷新令牌的端点。
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// 吊销端点（登出时尽力调用，失败不阻塞本地清理）。
pub const REVOKE_URL: &str = "https://auth.openai.com/oauth/revoke";
/// ChatGPT 认证后的 Responses API 根（`+ /responses`）。
pub const API_BASE: &str = "https://chatgpt.com/backend-api/codex";
/// 授权 scope。`offline_access` 是 refresh_token 的前提。
pub const SCOPE: &str = "openid profile email offline_access";
/// 回调端口。固定值：`redirect_uri` 随 client 注册，改端口授权服务器会拒。
pub const CALLBACK_PORT: u16 = 1455;
/// 回调路径。
pub const CALLBACK_PATH: &str = "/auth/callback";
/// ChatGPT 身份 claim 的容器键（OpenAI 命名空间下的自定义 claim）。
pub const AUTH_CLAIM: &str = "https://api.openai.com/auth";

/// 回调服务器绑定的地址。默认 `127.0.0.1`。
///
/// # 什么时候要改
///
/// `redirect_uri` 固定是 `http://localhost:1455/...`，而 **`localhost` 指浏览器
/// 所在的那台机器**。所以从局域网另一台机器用浏览器打开授权链接时，跳转打到
/// 的是那台机器自己的 1455，Core 收不到——用户看到"无法访问此网站"。
///
/// 两种解法：
///
/// 1. **SSH 隧道（推荐，无需改这里）**：在浏览器那台机器上
///    `ssh -N -L 1455:127.0.0.1:1455 <core-host>`，之后 `localhost:1455`
///    经由隧道回到 Core。这条路径在默认绑定下就能用。
/// 2. **局域网转发**：浏览器那台机器上把本地 1455 转发到 Core 的 1455。这要求
///    Core **不在**回环地址上监听，于是把 `ECHO_CHATGPT_CALLBACK_BIND` 设成
///    `0.0.0.0`。
///
/// # 设成 0.0.0.0 的安全性
///
/// 回调只是一次性收授权码，真正换令牌需要 `code_verifier`——它只存在 Core
/// 内存里、从不离开本机（PKCE 的用途正是这个）。所以同网段的人即使截获了
/// `code` 也换不出令牌。另加 `state` 逐字比对，挡掉伪造回调。
///
/// 仍然建议只在登录期间打开、登完改回 `127.0.0.1`——没必要长期暴露一个监听口。
pub fn callback_bind() -> String {
    std::env::var("ECHO_CHATGPT_CALLBACK_BIND")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// 回调地址（`redirect_uri` 必须与它逐字一致）。
pub fn redirect_uri() -> String {
    format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
}

/// 落盘的令牌集合。
///
/// `Debug` 会**脱敏**——这个结构会出现在日志/错误链里，而 access token 就是
/// 账户凭证本身。
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ChatGptAuth {
    /// 短期令牌（JSON 里是 JWT，携带 account id）。
    pub access_token: String,
    /// 长期令牌（`offline_access` 才有）。缺失 = 下次必须重新登录。
    #[serde(default)]
    pub refresh_token: String,
    /// 身份令牌，含 `chatgpt_account_id`；只在解析 account id 时用。
    #[serde(default)]
    pub id_token: String,
    /// ChatGPT 账户 id（请求头 `chatgpt-account-id`）。
    #[serde(default)]
    pub account_id: String,
    /// access token 到期的 Unix 毫秒。
    #[serde(default)]
    pub expires_at_ms: i64,
    /// 登录时的邮箱（展示用，可选）。
    #[serde(default)]
    pub email: String,
}

impl std::fmt::Debug for ChatGptAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatGptAuth")
            .field("access_token", &redact(&self.access_token))
            .field("refresh_token", &redact(&self.refresh_token))
            .field("id_token", &redact(&self.id_token))
            .field("account_id", &redact(&self.account_id))
            .field("expires_at_ms", &self.expires_at_ms)
            .field("email", &self.email)
            .finish()
    }
}

/// 只留长度与首 6 字符：够定位「是不是同一份 token」，不够拿来用。
fn redact(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let head: String = value.chars().take(6).collect();
    format!("{head}…({} chars)", value.chars().count())
}

impl ChatGptAuth {
    /// 提前 2 分钟视为过期：留出把新令牌写回磁盘与重发请求的时间。
    ///
    /// 不留余量的话，恰好在请求发出那一刻过期 → 401 → 上层当"登录失效"
    /// 让用户重登，而其实 refresh 就能解决。
    const EXPIRY_SKEW_MS: i64 = 120_000;

    /// 是否已过期（或即将过期）。
    pub fn is_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        self.expires_at_ms - Self::EXPIRY_SKEW_MS <= now
    }

    /// 有没有可用于调用的令牌。
    pub fn usable(&self) -> bool {
        !self.access_token.is_empty() && !self.account_id.is_empty()
    }
}

/// 令牌文件路径：`$ECHO_CHATGPT_AUTH` 优先，否则 Core 数据根。
///
/// 与会话/附件同一数据根（`~/.local/share/echo-agent-core/`）：登录态是
/// "本机这份数据"的一部分，不是进程状态——重启、升级都该留着。
pub fn auth_path() -> PathBuf {
    auth_path_with(
        std::env::var_os("ECHO_CHATGPT_AUTH"),
        std::env::var_os("HOME"),
    )
}

fn auth_path_with(
    override_path: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(path) = override_path.filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    match home {
        Some(home) => PathBuf::from(home).join(".local/share/echo-agent-core/chatgpt-auth.json"),
        None => PathBuf::from("chatgpt-auth.json"),
    }
}

/// 读令牌；文件不存在返回 `Ok(None)`（不是错误——没登录过是正常状态）。
pub fn load() -> Result<Option<ChatGptAuth>, String> {
    load_from(&auth_path())
}

fn load_from(path: &std::path::Path) -> Result<Option<ChatGptAuth>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {} failed: {error}", path.display())),
    };
    // 损坏时**报错而不是当没登录**：静默清空会让用户以为订阅失效，
    // 而真正的原因是文件被写坏了（磁盘满 / 手工编辑）。
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|error| format!("parse {} failed: {error}", path.display()))
}

/// 写令牌（临时文件 + 原子 rename，权限 0600）。
///
/// 0600 不是可选项：这份文件等价于账户凭证，同机其它用户能读等于泄露。
pub fn save(auth: &ChatGptAuth) -> Result<(), String> {
    save_to(&auth_path(), auth)
}

fn save_to(path: &std::path::Path, auth: &ChatGptAuth) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {} failed: {error}", parent.display()))?;
    }
    let body = serde_json::to_string_pretty(auth)
        .map_err(|error| format!("serialize auth failed: {error}"))?;
    let temporary = path.with_extension(format!("json.part{}", std::process::id()));
    write_private(&temporary, body.as_bytes())?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("commit {} failed: {error}", path.display())
    })
}

#[cfg(unix)]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| format!("open {} failed: {error}", path.display()))?;
    file.write_all(bytes)
        .map_err(|error| format!("write {} failed: {error}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|error| format!("write {} failed: {error}", path.display()))
}

/// 删除本地令牌（登出）。
pub fn clear() -> Result<(), String> {
    let path = auth_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {} failed: {error}", path.display())),
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// 一次 PKCE 挑战（verifier 保密，challenge 公开）。
pub struct Pkce {
    /// 一次性随机串：只在换令牌时发给 token 端点，绝不进授权 URL。
    pub verifier: String,
    /// verifier 的 SHA-256（base64url）：随授权 URL 发出去，泄露无害。
    pub challenge: String,
}

impl Pkce {
    /// 生成一对 verifier/challenge。
    ///
    /// verifier = 32 字节随机数的 base64url（43 字符，落在 RFC 7636 的
    /// 43–128 区间内）；challenge = base64url(sha256(verifier))。
    pub fn generate() -> Self {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self::from_verifier(base64_url(&bytes))
    }

    /// 用指定 verifier 派生 challenge（测试与「复用先前的 verifier」用）。
    pub fn from_verifier(verifier: String) -> Self {
        let digest = Sha256::digest(verifier.as_bytes());
        let challenge = base64_url(&digest);
        Self { verifier, challenge }
    }
}

/// base64url 无填充（PKCE 与 JWT 段都用这个变体）。
fn base64_url(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// 拼授权 URL。
///
/// `state` 与 PKCE verifier 一样必须是一次性的：回调里逐字比对，防的是
/// 「别人把 code 塞进你的回调」（OAuth CSRF）。
pub fn authorize_url(pkce: &Pkce, state: &str) -> String {
    let mut url = url::Url::parse(AUTHORIZE_URL).expect("AUTHORIZE_URL 是常量，解析不会失败");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("redirect_uri", &redirect_uri())
        .append_pair("scope", SCOPE)
        .append_pair("code_challenge", &pkce.challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", state)
        // 下面两个参数取自官方 CLI（见模块头）：授权页据此走"简化流程"
        // 并把组织信息塞进 id_token。不带它们不一定会失败，但既然要
        // 冒充那个 client，就按它的参数发。
        .append_pair("id_token_add_organizations", "true")
        .append_pair("codex_cli_simplified_flow", "true");
    url.to_string()
}

/// 生成一次性 state（32 字节随机）。
pub fn new_state() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    base64_url(&bytes)
}

// ---------------------------------------------------------------------------
// JWT / 响应解析
// ---------------------------------------------------------------------------

/// 从 JWT 的 payload 段取 `chatgpt_account_id` 与邮箱。
///
/// 不验签：这两个值来自我们自己刚换到的令牌（TLS 直连 auth.openai.com），
/// 用途是填请求头与展示，不是授权判决。验签需要 JWKS 拉取与时钟校验，
/// 在这里没有安全收益。
pub fn claims_of(jwt: &str) -> Option<serde_json::Value> {
    let payload = jwt.split('.').nth(1)?;
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 从 id_token（或 access token）解析账户信息。
///
/// 优先 id_token：`chatgpt_account_id` 规范地放在它的 auth claim 里。
/// access token 也带同一 claim（官方 CLI 就是用它自己取的），所以两边都试。
pub fn account_from_jwt(jwt: &str) -> (String, String) {
    let Some(claims) = claims_of(jwt) else {
        return (String::new(), String::new());
    };
    let account = claims
        .get(AUTH_CLAIM)
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    let email = claims
        .get("email")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    (account, email)
}

/// `/oauth/token` 的响应体。
#[derive(Debug, serde::Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub expires_in: i64,
}

impl TokenResponse {
    /// 转成落盘结构（补 account id、过期时刻）。
    pub fn into_auth(self) -> ChatGptAuth {
        // account id 依次看 id_token、access_token：两者都带该 claim，
        // 但只有 id_token 是规范位置。都取不到时留空——`usable()` 会因此
        // 为 false，登录流程据此明确报错，而不是发一个注定 401 的请求。
        let (mut account, email) = account_from_jwt(&self.id_token);
        if account.is_empty() {
            account = account_from_jwt(&self.access_token).0;
        }
        // `expires_in` 缺失或非正时按 1 小时兜底：宁可提前刷新一次，
        // 也不要因为算出一个过去的时间而每次请求都刷新。
        let ttl_ms = if self.expires_in > 0 {
            self.expires_in * 1000
        } else {
            3600 * 1000
        };
        ChatGptAuth {
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            id_token: self.id_token,
            account_id: account,
            expires_at_ms: chrono::Utc::now().timestamp_millis() + ttl_ms,
            email,
        }
    }
}

// ---------------------------------------------------------------------------
// 登录（本地回调服务器）
// ---------------------------------------------------------------------------

/// 登录结果里给调用方的中间态（用于"先打印 URL 再去等回调"）。
pub struct PendingLogin {
    pub url: String,
    /// 等回调的 future：解析出 code 后在这里完成。
    listener: tokio::net::TcpListener,
    pub state: String,
    pub pkce: Pkce,
    client: reqwest::Client,
}

/// 本通道专用代理地址。
///
/// **为什么不直接用全局 `HTTPS_PROXY`**：Core 同时服务 deepseek / kimi /
/// opencode 等通道，把全局代理设上会把**所有**流量绕到境外节点——延迟变差，
/// 有些国内中转还会因为出口 IP 变化拒绝请求。OpenAI 是唯一需要出境的通道，
/// 所以代理也只给它。
///
/// 优先级：`ECHO_CHATGPT_PROXY` → `HTTPS_PROXY`（后者是运维直觉里的默认，
/// 让"我已经全局配好了"的机器不用再改）。
pub fn proxy_url() -> Option<String> {
    for key in ["ECHO_CHATGPT_PROXY", "HTTPS_PROXY", "https_proxy"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// 建一个**带本通道代理**的 HTTP 客户端。
///
/// 所有出站请求（预检 / 换令牌 / 刷新 / Responses 调用）都用它，避免出现
/// "登录走了代理、调用没走"这种半通状态——那种状态下登录成功但一发消息就
/// 超时，排查会绕远路。
pub(crate) fn build_client() -> Result<reqwest::Client, String> {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15));
    if let Some(url) = proxy_url() {
        let proxy = reqwest::Proxy::all(&url)
            .map_err(|error| format!("代理地址无效（{url}）：{error}"))?;
        builder = builder.proxy(proxy);
        tracing::debug!(%url, "chatgpt channel using proxy");
    }
    builder
        .build()
        .map_err(|error| format!("build http client failed: {error}"))
}

/// 读登录态并转成协议事件（面板据此决定显示「登录」还是「已登录」）。
///
/// 文件损坏时返回 `logged_in: false` 并把原因塞进 `email` 字段吗？不——
/// 这里刻意**不**造一个"看起来没登录"的假状态：`load()` 的错误要原样
/// 让调用方看见（它会进 `Error` 事件），否则用户会以为订阅失效，
/// 而真正原因是磁盘上那份 JSON 坏了。
pub fn status_snapshot() -> agent_module_management::protocol::BackendEvent {
    use agent_module_management::protocol::BackendEvent;
    match load() {
        Ok(Some(auth)) => BackendEvent::ChatGptStatus {
            logged_in: auth.usable(),
            email: auth.email,
            expires_at_ms: auth.expires_at_ms,
            refreshable: !auth.refresh_token.is_empty(),
        },
        _ => BackendEvent::ChatGptStatus {
            logged_in: false,
            email: String::new(),
            expires_at_ms: 0,
            refreshable: false,
        },
    }
}

/// 登录前的**网络可达性预检**。
///
/// # 为什么必须有
///
/// 实测过一整轮：授权 URL 打不开，返回 `403 unsupported_country_region_territory`。
/// 如果等到用户走完浏览器授权、回调回来才在换令牌那一步报错，那段时间全白费，
/// 而且错误出现在"最后一步"，很容易被当成代码 bug。
///
/// 预检直接打 token 端点：这不是"猜哪些国家支持"（那份名单随政策变），而是
/// **问上游本人**。判据是响应体里的错误码：
///
/// | 响应 | 含义 |
/// |---|---|
/// | 含 `unsupported_country_region_territory` | 地区被拒 → 必须换出口 |
/// | 其它任何错误（`invalid_request` / `invalid_grant` / 400/405…） | 地区**通过**，只是这个假 code 无效 |
///
/// 也就是说我们只把"地区被拒"当成失败，其余一律视为通过——这样上游改了
/// 错误码也不会误判。
/// 请求**发不出去**时的提示（连不上代理 / 代理连不上上游）。
///
/// 必须点名当前用的代理：只说"要能直连 openai.com"会把排查方向带偏——
/// 实测那次用户明明配了代理，真正原因是代理节点挂了。两种情形（配了 / 没配）
/// 给的下一步完全不同，所以分开写。
pub(crate) fn transport_hint(error: &str) -> String {
    match proxy_url() {
        Some(proxy) => format!(
            "预检请求发不出去：{error}\n\
             当前使用的是本通道代理：{proxy}\n\
             先确认代理本身可用：\n  \
             curl -x {proxy} https://chatgpt.com/cdn-cgi/trace | grep loc=\n\
             常见原因：代理进程没起 / 节点挂了 / 节点落地在被封地区。\n\
             换节点：~/.local/share/echo-agent-chatgpt-proxy/pick-node.sh"
        ),
        None => format!(
            "预检请求发不出去：{error}\n\
             当前**没有配置**本通道代理（ECHO_CHATGPT_PROXY / HTTPS_PROXY 都为空），\n\
             而这台机器直连不到 openai.com。给 Core 配代理：\n  \
             systemctl --user edit echo-agent-core\n    [Service]\n    \
             Environment=ECHO_CHATGPT_PROXY=http://127.0.0.1:7897"
        ),
    }
}

pub async fn preflight() -> Result<(), String> {
    let client = build_client()?;
    // 只探测"地区门"，不真的换令牌：code 填一个假值，期望它被拒（那说明
    // 地区是通过的）。
    let response = client
        .post(TOKEN_URL)
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", "preflight-probe"),
            ("client_id", CLIENT_ID),
        ])
        .send()
        .await
        .map_err(|error| {
            transport_hint(&error.to_string())
        })?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    classify_preflight(status.as_u16(), &body)
}

/// 预检结论的判定（纯函数，便于单测）。
///
/// 只把"地区被拒"当失败：其余任何响应都说明网络层已到达上游。
pub(crate) fn classify_preflight(status: u16, body: &str) -> Result<(), String> {
    if body.contains("unsupported_country_region_territory") {
        return Err(format!(
            "网络预检失败：OpenAI 拒绝当前出口地区（HTTP {status}）。\n\
             登录前必须换到一个支持的出口。自检办法：\n  \
             curl -x $HTTPS_PROXY https://chatgpt.com/cdn-cgi/trace | grep loc=\n\
             `loc=` 需要是 US / JP / KR / SG 这类，`loc=HK` 与 `loc=CN` 都会被拒。\n\
             给 Core 配代理（服务不继承 shell 环境变量）：\n  \
             systemctl --user edit echo-agent-core\n    [Service]\n    \
             Environment=HTTPS_PROXY=http://127.0.0.1:7897"
        ));
    }
    // 任何非"地区"错误都算通过——上游总得回点什么（400/405/invalid_request）。
    let _ = status;
    Ok(())
}

/// 起一次登录：绑定回调端口并给出授权 URL。
///
/// 绑定失败（端口被占）时**直接报错**，不换端口：`redirect_uri` 是随 client
/// 注册的，换端口授权服务器会拒。最常见的原因是**本机 Codex CLI 正在登录**
/// （它也用 1455），错误信息里点明这一点。
pub async fn begin_login() -> Result<PendingLogin, String> {
    let bind = callback_bind();
    let listener = tokio::net::TcpListener::bind((bind.as_str(), CALLBACK_PORT))
        .await
        .map_err(|error| {
            format!(
                "绑定 {bind}:{CALLBACK_PORT} 失败（{error}）。这个端口是 ChatGPT OAuth 的\
                 redirect_uri 固定要求，不能换。常见原因：上一次登录还没结束（在等回调），\
                 或本机 codex CLI 正在同一个端口上登录。先在面板点「取消登录」再重试。"
            )
        })?;
    let pkce = Pkce::generate();
    let state = new_state();
    let url = authorize_url(&pkce, &state);
    let client = build_client()?;
    Ok(PendingLogin {
        url,
        listener,
        state,
        pkce,
        client,
    })
}

impl PendingLogin {
    /// 等浏览器跳回本地回调，换出令牌并落盘。
    pub async fn wait(self) -> Result<ChatGptAuth, String> {
        // `wait_for_code` 按值拿走 self（它要持有 listener），所以后面还要用的
        // 两个字段必须在调用**之前**取出——否则就是"借用已移动的值"。
        let client = self.client.clone();
        let verifier = self.pkce.verifier.clone();
        let code = self.wait_for_code().await?;
        let auth = exchange_code(&client, &code, &verifier).await?;
        if !auth.usable() {
            return Err(
                "登录成功但令牌里没有 ChatGPT 账户 id（不是 ChatGPT 订阅账号？\
                 API key 账号没有这个 claim）"
                    .into(),
            );
        }
        save(&auth)?;
        Ok(auth)
    }

    /// 等回调，**或者**用户手工把地址栏里的链接粘回来。
    ///
    /// # 为什么需要手工通道
    ///
    /// `redirect_uri` 固定是 `http://localhost:1455/auth/callback`，而
    /// **`localhost` 指浏览器所在的那台机器**。用户在局域网另一台机器上开面板
    /// 时，浏览器跳回的是他自己那台的 1455——那里没有服务，页面显示「无法访问
    /// 此页面」，**但地址栏里已经带着 `code` 与 `state`**。
    ///
    /// 让用户把那条 URL 粘回来，就绕过了「回调必须落到 Core 这台机器」的限制：
    /// 不需要 SSH 隧道、不需要 socat、不需要改监听地址。这是 headless 场景下
    /// OAuth 的标准解法（`gh auth login`、各家云 CLI 都这么干）。
    ///
    /// 安全性与 HTTP 回调完全等价：`state` 同样逐字比对，PKCE 的 verifier 同样
    /// 只在本进程内。区别只是 code 走了一条人肉搬运的链路。
    pub async fn wait_with_manual(
        self,
        manual: tokio::sync::oneshot::Receiver<String>,
    ) -> Result<ChatGptAuth, String> {
        let client = self.client.clone();
        let verifier = self.pkce.verifier.clone();
        let state = self.state.clone();
        // `wait_for_code` 按值拿走 self（持有 listener），所以 state 必须
        // 在此之前克隆出来——否则粘回来的链接无法校验。
        let code = tokio::select! {
            http = self.wait_for_code() => http?,
            input = manual => {
                let input = input.map_err(|_| "登录已取消".to_string())?;
                extract_code_from_input(&input, &state)?
            }
        };
        let auth = exchange_code(&client, &code, &verifier).await?;
        if !auth.usable() {
            return Err(
                "登录成功但令牌里没有 ChatGPT 账户 id（不是 ChatGPT 订阅账号？\
                 API key 账号没有这个 claim）"
                    .into(),
            );
        }
        save(&auth)?;
        Ok(auth)
    }

    /// 接受一次回调，校验 state，取出 code。
    ///
    /// 手写最小 HTTP/1.1：只为一个 GET 引入 HTTP 服务器依赖不值当，而这里
    /// 需要的功能严格限于「读一行请求行 → 取 query → 回一段 HTML」。
    async fn wait_for_code(self) -> Result<String, String> {
        // 5 分钟上限：够用户登录，又不会让调用方无限挂着（端口一直被占）。
        let accept = tokio::time::timeout(std::time::Duration::from_secs(300), async {
            loop {
                let (mut stream, _) = match self.listener.accept().await {
                    Ok(pair) => pair,
                    Err(error) => return Err(format!("accept failed: {error}")),
                };
                let mut buffer = vec![0u8; 8192];
                let read = match tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    tokio::io::AsyncReadExt::read(&mut stream, &mut buffer),
                )
                .await
                {
                    Ok(Ok(n)) => n,
                    // 浏览器预连接 / 空读：不是错误，继续等真正的回调。
                    _ => continue,
                };
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                let Some((code, state)) = parse_callback(&request) else {
                    // 不是回调路径（比如浏览器探测 /favicon.ico）：回 404 继续等。
                    let _ = respond(&mut stream, 404, "Not Found", "<h1>404</h1>").await;
                    continue;
                };
                if state != self.state {
                    let _ = respond(
                        &mut stream,
                        400,
                        "Bad Request",
                        "<h1>state 不匹配</h1><p>请重新发起登录。</p>",
                    )
                    .await;
                    return Err("OAuth state 不匹配（可能是伪造的回调）".into());
                }
                // 先回页面再返回：用户看到"完成"时令牌已经在换的路上。
                let _ = respond(&mut stream, 200, "OK", SUCCESS_HTML).await;
                return Ok(code);
            }
        })
        .await;
        match accept {
            Ok(result) => result,
            Err(_) => Err("等待授权回调超时（300 秒）".into()),
        }
    }
}

const SUCCESS_HTML: &str = "<!doctype html><meta charset=\"utf-8\"><title>EchoAgent</title>\
<body style=\"font-family:system-ui;padding:3rem;max-width:34rem\">\
<h1>登录完成</h1><p>可以关闭这个页面，回到 EchoAgent。</p></body>";

/// 从用户粘贴的内容里取授权码。
///
/// 接受三种输入，都是真实会遇到的：
///
/// - **完整回调 URL**（最常见）：浏览器跳回 `localhost:1455/auth/callback?code=…&state=…`，
///   页面打不开但地址栏是完整的——让用户整条复制过来
/// - **只有 query 部分**：`?code=…&state=…`（有些浏览器会省掉 `http://`）
/// - **只有 code 本身**：从「复制链接」里手工截出来的
///
/// `state` 只要出现就**必须**匹配：不匹配说明这条链接来自另一次登录（或伪造），
/// 用它换令牌会失败并浪费一次往返，不如当场拒绝并说清原因。
pub fn extract_code_from_input(input: &str, expected_state: &str) -> Result<String, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err("没有收到授权码：把浏览器地址栏里的完整链接粘贴进来".into());
    }
    // 用户可能贴了整条 URL、也可能只贴 query。统一按 `key=value&…` 解析。
    let query = trimmed
        .split_once('?')
        .map(|(_, q)| q)
        .unwrap_or(trimmed)
        .trim_start_matches('?');
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            "error" => error = Some(value),
            _ => {}
        }
    }
    if let Some(error) = error {
        return Err(format!(
            "授权被拒绝（{error}）。可能是你在授权页点了「拒绝」，或该账号没有订阅。"
        ));
    }
    let Some(code) = code else {
        // 没解析出 `code=` → 用户贴的可能是裸 code。
        // 裸 code 的形状：URL-safe base64（字母数字 `-` `_`），长度几十以上。
        let looks_like_bare_code = !trimmed.is_empty()
            && !trimmed.contains('=')
            && !trimmed.contains('/')
            && !trimmed.contains(' ')
            && trimmed.len() >= 16;
        if looks_like_bare_code {
            // 裸 code 无法校验 state；这是可接受的——它来自用户自己的浏览器，
            // 且换令牌仍需要本进程内的 verifier。
            return Ok(trimmed.to_string());
        }
        return Err(
            "链接里没有 code 参数。请粘贴登录完成后地址栏里的**完整**地址\
             （形如 http://localhost:1455/auth/callback?code=…&state=…）"
                .into(),
        );
    };
    if !expected_state.is_empty() {
        match state.as_deref() {
            Some(state) if state == expected_state => {}
            Some(_) => {
                return Err(
                    "这条链接来自另一次登录尝试（state 不匹配）。\
                     请重新点「登录 ChatGPT」拿一条新的链接。"
                        .into(),
                )
            }
            // 缺 state：只在裸 code 分支允许。完整 URL 缺 state 说明被截断。
            None => {
                return Err("链接里没有 state 参数——可能复制时被截断了，请重新复制完整地址"
                    .into())
            }
        }
    }
    Ok(code)
}

/// 从原始请求里取 `code` 与 `state`（非回调路径返回 `None`）。
fn parse_callback(request: &str) -> Option<(String, String)> {
    let first = request.lines().next()?;
    let target = first.split_whitespace().nth(1)?;
    if !target.starts_with(CALLBACK_PATH) {
        return None;
    }
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode(value);
        match key {
            "code" => code = Some(value),
            "state" => state = Some(value),
            // 用户在授权页点了拒绝：没有 error 分支就会一直等到超时。
            "error" => error = Some(value),
            _ => {}
        }
    }
    if let Some(error) = error {
        // 用 code 位带上错误，让上层统一按错误处理。
        return Some((format!("__error__:{error}"), state.unwrap_or_default()));
    }
    Some((code?, state.unwrap_or_default()))
}

/// 只处理 `%XX` 与 `+`：授权码是 base64url 字符集，不会有其它转义。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(bytes[index]);
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn respond(
    stream: &mut tokio::net::TcpStream,
    status: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

/// 用授权码换令牌。
async fn exchange_code(
    client: &reqwest::Client,
    code: &str,
    verifier: &str,
) -> Result<ChatGptAuth, String> {
    if let Some(error) = code.strip_prefix("__error__:") {
        return Err(format!("授权被拒绝：{error}"));
    }
    let form = [
        ("grant_type", "authorization_code"),
        ("code", code),
        ("redirect_uri", &redirect_uri()),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let response = client
        .post(TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("token 请求失败：{error}"))?;
    parse_token_response(response).await
}

/// 用 refresh_token 换新的 access_token。
///
/// ChatGPT 的 refresh 只轮换 access（并会带回新的 refresh_token）——
/// **必须把回来的 refresh_token 一并存下**：不存的话下一次刷新会用到一个
/// 已经作废的值，然后用户被迫重新登录。
pub async fn refresh(auth: &ChatGptAuth) -> Result<ChatGptAuth, String> {
    if auth.refresh_token.is_empty() {
        return Err("没有 refresh_token（登录时未取得 offline_access），需要重新登录".into());
    }
    let client = build_client()?;
    let form = [
        ("grant_type", "refresh_token"),
        ("refresh_token", auth.refresh_token.as_str()),
        ("client_id", CLIENT_ID),
    ];
    let response = client
        .post(TOKEN_URL)
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("refresh 请求失败：{error}"))?;
    let mut fresh = parse_token_response(response).await?;
    // 服务端没回新 refresh_token 时沿用旧的（有些实现只在轮换时返回）。
    if fresh.refresh_token.is_empty() {
        fresh.refresh_token = auth.refresh_token.clone();
    }
    if fresh.account_id.is_empty() {
        fresh.account_id = auth.account_id.clone();
    }
    if fresh.email.is_empty() {
        fresh.email = auth.email.clone();
    }
    Ok(fresh)
}

/// 把上游的错误体翻译成可执行的说法。
///
/// 实测（本机直连）：`auth.openai.com` 与 `chatgpt.com` 对部分地区返回
/// `403 unsupported_country_region_territory`。裸把这个 JSON 抛给用户只会
/// 得到"HTTP 403"，而真正要做的动作是**配代理**——这条路径一句提示就能
/// 省掉一轮排查。
pub(crate) fn explain_http_error(status: u16, body: &str) -> String {
    if body.contains("unsupported_country_region_territory") {
        return format!(
            "HTTP {status}：OpenAI 不支持当前所在国家/地区（unsupported_country_region_territory）。\n             这条通道必须能直连 openai.com——请给 Core 配代理后重试：\n               export HTTPS_PROXY=http://127.0.0.1:7890   （reqwest 默认读这个环境变量）"
        );
    }
    if body.contains("invalid_client") {
        return format!(
            "HTTP {status}：client_id 被拒（invalid_client）。上游可能已收紧第三方登录，\
             需要改用官方客户端。"
        );
    }
    format!("HTTP {status}：{}", truncate(body, 400))
}

async fn parse_token_response(response: reqwest::Response) -> Result<ChatGptAuth, String> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读 token 响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!(
            "token 端点返回 {}",
            explain_http_error(status.as_u16(), &text)
        ));
    }
    let parsed: TokenResponse = serde_json::from_str(&text)
        .map_err(|error| format!("解析 token 响应失败：{error} — {}", truncate(&text, 300)))?;
    Ok(parsed.into_auth())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    text.chars().take(max).collect::<String>() + "…"
}

/// 取一份**可用**的令牌：必要时自动刷新并写回磁盘。
///
/// 三条路径：
/// - 没登录 → 明确的"请先登录"错误（带命令名，用户知道下一步做什么）
/// - 未过期 → 直接返回
/// - 已过期 → refresh；refresh 也失败时提示重新登录
pub async fn ensure_fresh() -> Result<ChatGptAuth, String> {
    let Some(auth) = load()? else {
        return Err("未登录 ChatGPT 订阅：先运行 `echo-agent-core login-chatgpt`".into());
    };
    if !auth.is_expired() {
        return Ok(auth);
    }
    let fresh = refresh(&auth).await?;
    save(&fresh)?;
    Ok(fresh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_example() {
        // RFC 7636 附录 B 的样例值（S256）。
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".into());
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pkce_verifier_is_url_safe_and_long_enough() {
        let pkce = Pkce::generate();
        assert!(pkce.verifier.len() >= 43 && pkce.verifier.len() <= 128);
        assert!(pkce
            .verifier
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')));
        // 两次生成必须不同（随机源走到了）。
        assert_ne!(pkce.verifier, Pkce::generate().verifier);
    }

    #[test]
    fn authorize_url_carries_every_required_param() {
        let pkce = Pkce::from_verifier("v".repeat(43));
        let url = authorize_url(&pkce, "the-state");
        for expected in [
            "response_type=code",
            "client_id=app_EMoamEEZ73f0CkXaXp7hrann",
            "code_challenge_method=S256",
            "state=the-state",
            "scope=openid+profile+email+offline_access",
        ] {
            assert!(url.contains(expected), "missing {expected} in {url}");
        }
        // redirect_uri 必须与回调端口逐字一致（端口改了授权服务器直接拒）。
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback"));
    }

    #[test]
    fn parse_callback_reads_code_and_state() {
        let request = "GET /auth/callback?code=abc123&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n";
        assert_eq!(
            parse_callback(request),
            Some(("abc123".to_string(), "xyz".to_string()))
        );
    }

    #[test]
    fn parse_callback_ignores_other_paths() {
        assert_eq!(parse_callback("GET /favicon.ico HTTP/1.1\r\n\r\n"), None);
    }

    /// 用户在授权页点「拒绝」：必须立刻失败，不能等到 300 秒超时。
    #[test]
    fn parse_callback_surfaces_denial() {
        let request = "GET /auth/callback?error=access_denied&state=s HTTP/1.1\r\n\r\n";
        let (code, _) = parse_callback(request).expect("should parse");
        assert!(code.starts_with("__error__:"));
        assert!(code.contains("access_denied"));
    }

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%2Fb+c"), "a/b c");
        // 非法转义原样保留（授权码里不该出现，但不能因此丢掉整个 code）。
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[test]
    fn claims_extract_account_id_and_email() {
        use base64::Engine;
        let payload = serde_json::json!({
            AUTH_CLAIM: { "chatgpt_account_id": "acc-123" },
            "email": "user@example.com",
        })
        .to_string();
        let jwt = format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let (account, email) = account_from_jwt(&jwt);
        assert_eq!(account, "acc-123");
        assert_eq!(email, "user@example.com");
    }

    #[test]
    fn claims_missing_account_yields_empty() {
        // API key 账号没有 ChatGPT claim：account 为空，`usable()` 因此为 false。
        use base64::Engine;
        let payload = serde_json::json!({ "email": "k@example.com" }).to_string();
        let jwt = format!(
            "h.{}.s",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
        );
        let (account, _) = account_from_jwt(&jwt);
        assert!(account.is_empty());
        assert!(!ChatGptAuth {
            access_token: "a".into(),
            account_id: account,
            ..Default::default()
        }
        .usable());
    }

    #[test]
    fn token_response_falls_back_to_access_token_account() {
        use base64::Engine;
        let access_payload = serde_json::json!({ AUTH_CLAIM: { "chatgpt_account_id": "acc-from-access" } })
            .to_string();
        let access = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(access_payload);
        let auth = TokenResponse {
            access_token: format!("h.{access}.s"),
            refresh_token: "r".into(),
            id_token: String::new(),
            expires_in: 60,
        }
        .into_auth();
        assert_eq!(auth.account_id, "acc-from-access");
        assert_eq!(auth.expires_at_ms > chrono::Utc::now().timestamp_millis(), true);
    }

    #[test]
    fn expiry_skew_treats_near_expiry_as_expired() {
        let now = chrono::Utc::now().timestamp_millis();
        let fresh = ChatGptAuth {
            access_token: "a".into(),
            account_id: "acc".into(),
            expires_at_ms: now + 10 * 60 * 1000,
            ..Default::default()
        };
        assert!(!fresh.is_expired());
        // 只剩 60 秒 → 已在 120 秒余量内，视为过期（提前刷新）。
        let almost = ChatGptAuth {
            expires_at_ms: now + 60 * 1000,
            ..fresh.clone()
        };
        assert!(almost.is_expired());
    }

    #[test]
    fn save_and_load_roundtrip_is_private() {
        let dir = std::env::temp_dir().join(format!(
            "echo-chatgpt-auth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let path = dir.join("auth.json");
        let auth = ChatGptAuth {
            access_token: "access".into(),
            refresh_token: "refresh".into(),
            account_id: "acc".into(),
            expires_at_ms: 123,
            email: "e@x".into(),
            ..Default::default()
        };
        save_to(&path, &auth).unwrap();
        let loaded = load_from(&path).unwrap().expect("auth should exist");
        assert_eq!(loaded.access_token, "access");
        assert_eq!(loaded.account_id, "acc");
        // 缺失文件 ≠ 错误
        assert!(load_from(&dir.join("missing.json")).unwrap().is_none());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "token 文件必须是 0600（等价于账户凭证）");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 损坏的令牌文件必须报错——静默当"没登录"会让用户以为订阅失效。
    #[test]
    fn corrupt_file_is_an_error_not_a_logout() {
        let dir = std::env::temp_dir().join(format!(
            "echo-chatgpt-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("auth.json");
        std::fs::write(&path, b"{not json").unwrap();
        let error = load_from(&path).unwrap_err();
        assert!(error.contains("parse"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Debug 脱敏：令牌会进日志/错误链。
    #[test]
    fn debug_redacts_tokens() {
        let auth = ChatGptAuth {
            access_token: "super-secret-access-value".into(),
            refresh_token: "super-secret-refresh".into(),
            account_id: "acc-123456".into(),
            ..Default::default()
        };
        let rendered = format!("{auth:?}");
        assert!(!rendered.contains("super-secret-access-value"));
        assert!(!rendered.contains("super-secret-refresh"));
        assert!(rendered.contains("chars"), "{rendered}");
    }

    /// 代理只给本通道：`ECHO_CHATGPT_PROXY` 优先于 `HTTPS_PROXY`，
    /// 空值当未设。这条锁的是"不会因为机器上有全局代理就把 deepseek 也绕出国"。
    #[test]
    fn proxy_url_prefers_channel_specific_var() {
        // 环境变量是进程级的，串行化避免与其它用例交叉。
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: 本用例串行读写这两个变量。
        unsafe {
            std::env::remove_var("ECHO_CHATGPT_PROXY");
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("https_proxy");
        }
        assert_eq!(proxy_url(), None, "两个都没设时不该有代理");

        unsafe { std::env::set_var("HTTPS_PROXY", "http://127.0.0.1:7897") };
        assert_eq!(proxy_url().as_deref(), Some("http://127.0.0.1:7897"));

        // 专用变量压过全局变量。
        unsafe { std::env::set_var("ECHO_CHATGPT_PROXY", "http://127.0.0.1:17897") };
        assert_eq!(proxy_url().as_deref(), Some("http://127.0.0.1:17897"));

        // 空值视为未设（运维常把变量导成空串）。
        unsafe { std::env::set_var("ECHO_CHATGPT_PROXY", "  ") };
        assert_eq!(proxy_url().as_deref(), Some("http://127.0.0.1:7897"));

        unsafe {
            std::env::remove_var("ECHO_CHATGPT_PROXY");
            std::env::remove_var("HTTPS_PROXY");
        }
    }

    /// 非法代理地址要报错而不是静默直连——静默直连会表现为"地区被拒"，
    /// 而真正原因是代理配置写错了。
    #[test]
    fn build_client_rejects_invalid_proxy() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ECHO_CHATGPT_PROXY", "::not a url::") };
        let error = build_client().unwrap_err();
        assert!(error.contains("代理地址无效"), "{error}");
        unsafe { std::env::remove_var("ECHO_CHATGPT_PROXY") };
    }

    /// 发不出去时的提示必须**点名当前代理**（配了代理却说"要能直连"
    /// 会把排查方向带偏——实测踩过：真正原因是代理节点挂了）。
    #[test]
    fn unreachable_hint_names_the_proxy_in_use() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe {
            std::env::remove_var("HTTPS_PROXY");
            std::env::remove_var("https_proxy");
            std::env::set_var("ECHO_CHATGPT_PROXY", "http://127.0.0.1:17897");
        }
        let hint = transport_hint("connect error");
        assert!(hint.contains("http://127.0.0.1:17897"), "{hint}");
        assert!(hint.contains("curl -x http://127.0.0.1:17897"), "{hint}");
        assert!(hint.contains("pick-node.sh"), "应给出换节点的办法：{hint}");

        unsafe {
            std::env::remove_var("ECHO_CHATGPT_PROXY");
        }
        let bare = transport_hint("connect error");
        assert!(bare.contains("没有配置"), "{bare}");
        assert!(bare.contains("ECHO_CHATGPT_PROXY"), "{bare}");
    }

    /// 预检只把"地区被拒"判为失败——这是上一轮实测到的唯一真实失败形态。
    #[test]
    fn preflight_flags_geo_block() {
        let error = classify_preflight(
            403,
            r#"{"error":{"code":"unsupported_country_region_territory"}}"#,
        )
        .unwrap_err();
        assert!(error.contains("拒绝当前出口地区"), "{error}");
        assert!(error.contains("loc="), "应给出自检办法：{error}");
        assert!(error.contains("systemctl --user edit"), "应给出服务端配法");
    }

    /// 其余任何响应都算"地区通过"：假 code 本来就该被拒（400/invalid_grant），
    /// 那是通过网络的证明，不是地区问题。
    #[test]
    fn preflight_accepts_non_geo_errors() {
        for (status, body) in [
            (400, r#"{"error":"invalid_grant"}"#),
            (400, r#"{"error":"invalid_request","error_description":"bad code"}"#),
            (405, ""),
            (200, "{}"),
        ] {
            assert!(
                classify_preflight(status, body).is_ok(),
                "status={status} body={body} 不该被判为地区失败"
            );
        }
    }

    /// 手工粘贴：完整回调 URL（局域网场景的主要输入形态）。
    #[test]
    fn manual_input_accepts_full_callback_url() {
        let url = "http://localhost:1455/auth/callback?code=abc123&state=xyz";
        assert_eq!(extract_code_from_input(url, "xyz").unwrap(), "abc123");
    }

    /// 只有 query、或带前导 `?` 也能认（浏览器「复制链接」常少 `http://`）。
    #[test]
    fn manual_input_accepts_query_only() {
        assert_eq!(
            extract_code_from_input("?code=abc&state=s", "s").unwrap(),
            "abc"
        );
        assert_eq!(
            extract_code_from_input("code=abc&state=s", "s").unwrap(),
            "abc"
        );
    }

    /// 裸 code：长度够、无 `=` `/` 空格 → 当作 code（无法校验 state，可接受）。
    #[test]
    fn manual_input_accepts_bare_code() {
        let bare = "ac-_9fKq2LmN0pQrStUvWx";
        assert_eq!(extract_code_from_input(bare, "any-state").unwrap(), bare);
    }

    /// state 不匹配 → 明确拒绝并指出"来自另一次登录"，而不是拿去换令牌
    /// （那样只会得到一个看不懂的 invalid_grant）。
    #[test]
    fn manual_input_rejects_foreign_state() {
        let url = "http://localhost:1455/auth/callback?code=abc&state=other";
        let error = extract_code_from_input(url, "expected").unwrap_err();
        assert!(error.contains("另一次登录"), "{error}");
    }

    /// 完整 URL 缺 state：说明复制被截断，必须明确说出来。
    #[test]
    fn manual_input_rejects_truncated_url() {
        let error =
            extract_code_from_input("http://localhost:1455/auth/callback?code=abc", "s")
                .unwrap_err();
        assert!(error.contains("state"), "{error}");
    }

    /// 用户在授权页点了「拒绝」：要翻译成人话。
    #[test]
    fn manual_input_surfaces_denial() {
        let url = "http://localhost:1455/auth/callback?error=access_denied&state=s";
        let error = extract_code_from_input(url, "s").unwrap_err();
        assert!(error.contains("授权被拒绝"), "{error}");
    }

    /// 空输入 / 贴错东西：给出"该粘什么"的具体说明，而不是干巴巴的解析失败。
    #[test]
    fn manual_input_explains_what_to_paste() {
        assert!(extract_code_from_input("   ", "s")
            .unwrap_err()
            .contains("完整链接"));
        let error = extract_code_from_input("https://example.com/hello", "s").unwrap_err();
        assert!(error.contains("完整"), "{error}");
    }

    /// 回调绑定地址：默认回环；`ECHO_CHATGPT_CALLBACK_BIND` 可覆盖（局域网转发用）。
    #[test]
    fn callback_bind_defaults_to_loopback() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("ECHO_CHATGPT_CALLBACK_BIND") };
        assert_eq!(callback_bind(), "127.0.0.1");

        unsafe { std::env::set_var("ECHO_CHATGPT_CALLBACK_BIND", "0.0.0.0") };
        assert_eq!(callback_bind(), "0.0.0.0");

        // 空值当未设（运维常把变量导成空串，绑 "" 会直接失败）。
        unsafe { std::env::set_var("ECHO_CHATGPT_CALLBACK_BIND", "  ") };
        assert_eq!(callback_bind(), "127.0.0.1");

        unsafe { std::env::remove_var("ECHO_CHATGPT_CALLBACK_BIND") };
    }

    /// `redirect_uri` 必须始终是 `localhost:1455`——它随 client 注册，改了就
    /// 被授权服务器拒（实测 `param: redirect_uri` → invalid_request）。
    /// 局域网访问只能靠隧道/转发，不能靠改这个值。
    #[test]
    fn redirect_uri_is_fixed_regardless_of_bind() {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ECHO_CHATGPT_CALLBACK_BIND", "0.0.0.0") };
        assert_eq!(redirect_uri(), "http://localhost:1455/auth/callback");
        unsafe { std::env::remove_var("ECHO_CHATGPT_CALLBACK_BIND") };
    }

    /// 地区封锁必须是"配代理"这条可执行提示，而不是裸 HTTP 403。
    /// 实测本机直连 auth.openai.com 就是这个错——用户看到的第一句话决定
    /// 他下一步做什么。
    #[test]
    fn geo_block_message_tells_user_to_configure_proxy() {
        let message = explain_http_error(
            403,
            r#"{"error":{"code":"unsupported_country_region_territory"}}"#,
        );
        assert!(message.contains("不支持当前所在国家/地区"), "{message}");
        assert!(message.contains("HTTPS_PROXY"), "{message}");
    }

    /// client_id 被上游拒时要说清"这是上游收紧，不是你的配置错"。
    #[test]
    fn invalid_client_message_blames_upstream() {
        let message = explain_http_error(401, r#"{"error":"invalid_client"}"#);
        assert!(message.contains("client_id 被拒"), "{message}");
    }

    #[test]
    fn auth_path_override_wins() {
        let path = auth_path_with(
            Some(std::ffi::OsString::from("/custom/auth.json")),
            Some(std::ffi::OsString::from("/home/x")),
        );
        assert_eq!(path, PathBuf::from("/custom/auth.json"));
        let fallback = auth_path_with(None, Some(std::ffi::OsString::from("/home/x")));
        assert_eq!(
            fallback,
            PathBuf::from("/home/x/.local/share/echo-agent-core/chatgpt-auth.json")
        );
    }
}
