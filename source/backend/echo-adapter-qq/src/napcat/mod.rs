//! NapCat WebUI API client.
//!
//! Provides programmatic access to NapCat's management API:
//! - Auto-configure reverse WebSocket client
//! - Check login / connection status
//! - Fetch QR code for TUI display
//! - Health check
//!
//! API base URL defaults to `http://localhost:6099` (NapCat WebUI).

use serde::Deserialize;

/// NapCat API client.
pub struct NapCatClient {
    base_url: String,
    /// OneBot HTTP API base (default `http://localhost:3000`, overridable for tests).
    onebot_url: String,
    client: reqwest::Client,
}

/// Login / connection status returned by NapCat.
#[derive(Debug, Clone, Deserialize)]
pub struct NapCatStatus {
    /// Whether QQ is logged in.
    #[serde(default)]
    pub online: bool,
    /// QQ user ID if logged in. Some NapCat versions report it as an
    /// integer — normalised to a string.
    #[serde(default, deserialize_with = "deserialize_optional_id")]
    pub user_id: Option<String>,
    /// QQ nickname.
    #[serde(default)]
    pub nickname: Option<String>,
    /// Raw status string for fallback.
    #[serde(default)]
    pub message: Option<String>,
}

/// Deserialise an optional user id that may arrive as string or integer.
fn deserialize_optional_id<'de, D>(d: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::Deserialize as _;
    let v = Option::<serde_json::Value>::deserialize(d)?;
    Ok(match v {
        Some(serde_json::Value::String(s)) => Some(s),
        Some(serde_json::Value::Number(n)) => Some(n.to_string()),
        Some(serde_json::Value::Null) | None => None,
        _ => None,
    })
}

fn json_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
}

/// NapCat connection result.
#[derive(Debug)]
pub enum NapCatState {
    /// QQ is logged in and connected.
    Online { user_id: String, nickname: String },
    /// NapCat is running but not logged into QQ.
    WaitingForLogin,
    /// NapCat is not reachable (container not running or port not exposed).
    Unreachable,
}

impl NapCatClient {
    /// Create a new NapCat API client.
    /// `webui_url` is the NapCat WebUI address, e.g. `http://localhost:6099`.
    pub fn new(webui_url: &str) -> Self {
        Self::with_onebot_url(webui_url, "http://localhost:3000")
    }

    /// Like [`new`](Self::new), with an explicit OneBot HTTP API URL
    /// (used by tests to point at a mock server).
    pub fn with_onebot_url(webui_url: &str, onebot_url: &str) -> Self {
        Self {
            base_url: webui_url.trim_end_matches('/').to_string(),
            onebot_url: onebot_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "reqwest builder failed, using default client");
                    reqwest::Client::new()
                }),
        }
    }

    // ---- health / status ----

    /// Check if NapCat is reachable and logged in.
    /// Returns `NapCatState` summarising the result.
    pub async fn check(&self) -> NapCatState {
        match self.login_status().await {
            Ok(status) if status.online => NapCatState::Online {
                user_id: status.user_id.unwrap_or_else(|| "?".into()),
                nickname: status.nickname.unwrap_or_else(|| "?".into()),
            },
            Ok(_) => NapCatState::WaitingForLogin,
            Err(_) => NapCatState::Unreachable,
        }
    }

    /// Query NapCat's login status.
    ///
    /// 优先使用 OneBot HTTP API（docker-compose 将容器 3000 端口映射到宿主机）。
    /// 这与 TUI 的 `/qq login` 完全一致，已验证能正确识别登录状态。
    /// 仅当 OneBot API 不可达时，才回退到 NapCat WebUI API。
    pub async fn login_status(&self) -> Result<NapCatStatus, String> {
        // 1) 优先 OneBot HTTP API —— 与 /qq login 行为一致
        match self.login_status_via_onebot().await {
            Ok(status) => return Ok(status),
            Err(e) => tracing::debug!(error = %e, "OneBot API unavailable, trying WebUI"),
        }

        // 2) 回退：NapCat WebUI /api/login/status
        // 注意：该接口的 data 里登录字段名（online/isLogin 等）在不同版本不一致，
        // 且未带 token 时可能返回 code!=0，因此仅作 fallback。
        let url = format!("{}/api/login/status", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        if resp.status().is_success() {
            let raw: serde_json::Value =
                resp.json().await.map_err(|e| format!("parse error: {e}"))?;
            let code = raw.get("code").and_then(|c| c.as_i64());
            if code == Some(0) {
                if let Some(data) = raw.get("data") {
                    let status: NapCatStatus = serde_json::from_value(data.clone())
                        .map_err(|e| format!("data parse error: {e}"))?;
                    if status.online {
                        return Ok(status);
                    }
                    tracing::warn!(body = %raw, "WebUI reports offline — trusting OneBot API result");
                }
            }
            tracing::warn!(body = %raw, "NapCat WebUI status response not usable");
        }
        // 3) 最终再尝试一次 OneBot API
        self.login_status_via_onebot().await
    }

    async fn login_status_via_onebot(&self) -> Result<NapCatStatus, String> {
        // OneBot HTTP API is typically on port 3000 inside the container.
        // Try localhost:3000 (common port mapping).
        let url = format!("{}/get_login_info", self.onebot_url);
        let resp = self
            .client
            .post(url)
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .map_err(|e| format!("OneBot HTTP error: {e}"))?;
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("OneBot parse error: {e}"))?;
        let data = &body["data"];
        let user_id = data["user_id"].as_i64().map(|id| id.to_string());
        let nickname = data["nickname"].as_str().map(|s| s.to_string());
        let online = user_id.is_some() && user_id.as_deref() != Some("0");
        Ok(NapCatStatus {
            online,
            user_id,
            nickname,
            message: Some("via OneBot API".into()),
        })
    }

    async fn onebot_action(&self, action: &str) -> Result<serde_json::Value, String> {
        let url = format!("{}/{action}", self.onebot_url);
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .body("{}")
            .send()
            .await
            .map_err(|e| format!("OneBot {action} HTTP error: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(format!("OneBot {action} returned HTTP {status}"));
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| format!("OneBot {action} parse error: {e}"))?;
        let retcode = body.get("retcode").and_then(json_i64).unwrap_or(0);
        if retcode != 0 || body.get("status").and_then(|v| v.as_str()) == Some("failed") {
            let message = body
                .get("message")
                .or_else(|| body.get("wording"))
                .and_then(|value| value.as_str())
                .unwrap_or("unknown OneBot error");
            return Err(format!("OneBot {action} failed ({retcode}): {message}"));
        }
        body.get("data")
            .cloned()
            .ok_or_else(|| format!("OneBot {action} response has no data"))
    }

    /// Fetch every group visible to the logged-in QQ account.
    pub async fn get_group_list(&self) -> Result<Vec<(i64, String)>, String> {
        let data = self.onebot_action("get_group_list").await?;
        let groups = data
            .as_array()
            .ok_or("OneBot get_group_list data is not an array")?
            .iter()
            .filter_map(|item| {
                let id = json_i64(item.get("group_id")?)?;
                let name = item
                    .get("group_name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                Some((id, name))
            })
            .collect();
        Ok(groups)
    }

    /// Fetch every friend visible to the logged-in QQ account.
    pub async fn get_friend_list(&self) -> Result<Vec<(i64, String)>, String> {
        let data = self.onebot_action("get_friend_list").await?;
        let friends = data
            .as_array()
            .ok_or("OneBot get_friend_list data is not an array")?
            .iter()
            .filter_map(|item| {
                let id = json_i64(item.get("user_id")?)?;
                let name = item
                    .get("nickname")
                    .and_then(|value| value.as_str())
                    .unwrap_or("(unknown)")
                    .to_string();
                Some((id, name))
            })
            .collect();
        Ok(friends)
    }

    // ---- reverse WebSocket configuration ----

    /// Auto-configure NapCat to connect back to our reverse WebSocket server.
    ///
    /// `ws_url` is the address NapCat should connect to, e.g. `ws://host:3131`.
    pub async fn configure_reverse_ws(&self, ws_url: &str, token: &str) -> Result<(), String> {
        let url = format!("{}/api/network/wsReverse", self.base_url);
        let body = serde_json::json!({
            "enabled": true,
            "url": ws_url,
            "type": "array",
            "token": token,
        });
        let resp = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;

        if resp.status().is_success() {
            tracing::info!(%ws_url, "NapCat reverse WS configured");
            Ok(())
        } else {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            tracing::warn!(%status, %body, "NapCat reverse WS config may have failed — verify in WebUI");
            // Don't fail hard — the config might have been applied anyway.
            Ok(())
        }
    }

    // ---- QR code ----

    /// Fetch the login QR code PNG from NapCat's container via Docker.
    /// Returns raw PNG bytes, or an error if Docker/NapCat is not available.
    pub fn fetch_qrcode_docker(container_name: &str) -> Result<Vec<u8>, String> {
        use std::process::Command;
        let output = Command::new("docker")
            .args([
                "exec",
                container_name,
                "cat",
                "/app/napcat/cache/qrcode.png",
            ])
            .output()
            .map_err(|e| format!("docker exec failed: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "docker exec failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        if output.stdout.is_empty() {
            return Err("QR code not yet available (waiting for NapCat to generate)".into());
        }
        Ok(output.stdout)
    }

    /// Try to fetch QR code via the WebUI API.
    pub async fn fetch_qrcode_web(&self) -> Result<Vec<u8>, String> {
        let url = format!("{}/api/qrcode", self.base_url);
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("HTTP error: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        resp.bytes()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("read error: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn onebot_mock(data: serde_json::Value) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/get_login_info"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "retcode": 0,
                "data": data,
            })))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn check_reports_online_when_onebot_has_user() {
        let server = onebot_mock(serde_json::json!({"user_id": 123456, "nickname": "bot"})).await;
        let client = NapCatClient::with_onebot_url("http://localhost:9999", &server.uri());
        match client.check().await {
            NapCatState::Online { user_id, nickname } => {
                assert_eq!(user_id, "123456");
                assert_eq!(nickname, "bot");
            }
            other => panic!("expected Online, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn check_reports_waiting_when_not_logged_in() {
        let server = onebot_mock(serde_json::json!({"user_id": 0})).await;
        let client = NapCatClient::with_onebot_url("http://localhost:9999", &server.uri());
        assert!(matches!(client.check().await, NapCatState::WaitingForLogin));
    }

    #[tokio::test]
    async fn check_reports_unreachable_when_both_apis_fail() {
        // OneBot mock that always fails + a WebUI that is unreachable.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/get_login_info"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = NapCatClient::with_onebot_url("http://localhost:1", &server.uri());
        assert!(matches!(client.check().await, NapCatState::Unreachable));
    }

    #[tokio::test]
    async fn login_status_falls_back_to_webui() {
        // OneBot fails; WebUI reports online via /api/login/status.
        let onebot = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/get_login_info"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&onebot)
            .await;

        let webui = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/login/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0,
                "data": {"online": true, "user_id": 777, "nickname": "webui-bot"}
            })))
            .mount(&webui)
            .await;

        let client = NapCatClient::with_onebot_url(&webui.uri(), &onebot.uri());
        let status = client.login_status().await.expect("fallback works");
        assert!(status.online);
        assert_eq!(status.user_id.as_deref(), Some("777"));
    }

    #[tokio::test]
    async fn get_group_list_returns_all_groups_with_numeric_or_string_ids() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/get_group_list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "ok",
                "retcode": 0,
                "data": [
                    {"group_id": 111, "group_name": "alpha"},
                    {"group_id": "222", "group_name": "beta"}
                ]
            })))
            .mount(&server)
            .await;
        let client = NapCatClient::with_onebot_url("http://localhost:1", &server.uri());

        assert_eq!(
            client.get_group_list().await.unwrap(),
            vec![(111, "alpha".into()), (222, "beta".into())]
        );
    }

    #[tokio::test]
    async fn get_group_list_reports_onebot_failure() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/get_group_list"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "failed",
                "retcode": 1404,
                "message": "not logged in",
                "data": null
            })))
            .mount(&server)
            .await;
        let client = NapCatClient::with_onebot_url("http://localhost:1", &server.uri());

        let error = client.get_group_list().await.unwrap_err();
        assert!(error.contains("not logged in"), "{error}");
    }

    #[tokio::test]
    async fn configure_reverse_ws_posts_config() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/network/wsReverse"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let client = NapCatClient::new(&server.uri());
        client
            .configure_reverse_ws("ws://host.docker.internal:3131", "tok123")
            .await
            .expect("config accepted");
    }

    #[tokio::test]
    async fn configure_reverse_ws_does_not_fail_hard_on_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/network/wsReverse"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = NapCatClient::new(&server.uri());
        // The API treats non-2xx as "verify in WebUI" — still Ok.
        client
            .configure_reverse_ws("ws://x:3131", "")
            .await
            .expect("lenient error handling");
    }

    #[tokio::test]
    async fn fetch_qrcode_web_returns_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/qrcode"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3, 4]))
            .mount(&server)
            .await;
        let client = NapCatClient::new(&server.uri());
        let bytes = client.fetch_qrcode_web().await.expect("qrcode");
        assert_eq!(bytes, vec![1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn fetch_qrcode_web_errors_on_non_success() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/qrcode"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let client = NapCatClient::new(&server.uri());
        assert!(client.fetch_qrcode_web().await.is_err());
    }
}
