//! `plugin-upgrade`：经本机 management WS 请求一个外部插件热替换。
//!
//! 部署器与 Core 是独立进程，不能直接拿 `PluginHandle`。复用既有
//! `RestartPlugin` 管理命令可避免第二套控制通道；运行中的新 Core 对该命令
//! 执行 replacement-handshake → 原子切换 → drain，而不是 stop/start。

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use echo_protocol::{BackendCommand, BackendEvent};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;

use crate::config::CoreConfig;

/// 将监听地址转换为本机客户端可连接的 WS URL。
/// 通配地址不能直接作为客户端目的地；回环地址保持原值。
fn management_url(address: &str) -> String {
    let address = address.trim();
    let address = address
        .strip_prefix("0.0.0.0:")
        .map(|port| format!("127.0.0.1:{port}"))
        .unwrap_or_else(|| address.to_string());
    if let Some(rest) = address.strip_prefix('[') {
        if rest.starts_with("::]:") {
            return format!("ws://[::1]:{}", &rest[4..]);
        }
    }
    if address.starts_with("ws://") || address.starts_with("wss://") {
        address
    } else {
        format!("ws://{address}")
    }
}

/// 按安装路径反查 plugins.toml 的运行时 id。crate/二进制名与插件 id
/// 不强制相同（例如 echo-plugin-example 的运行时 id 是 example）。
fn resolve_plugin_id(
    config_path: &Path,
    id: Option<&str>,
    binary: Option<&Path>,
) -> Result<String> {
    if let Some(id) = id {
        if id.trim().is_empty() {
            bail!("plugin id must not be empty");
        }
        return Ok(id.to_string());
    }
    let binary = binary.context("either plugin id or --binary is required")?;
    let wanted = binary.canonicalize().unwrap_or_else(|_| binary.to_path_buf());
    let tree = echo_plugin_loader::load(&config_path.with_file_name("plugins.toml"))
        .context("load plugins.toml for --binary lookup")?;
    let matches: Vec<&str> = tree
        .entries
        .iter()
        .filter(|entry| {
            entry.command.as_deref().is_some_and(|command| {
                let path = Path::new(command);
                path.canonicalize().unwrap_or_else(|_| path.to_path_buf()) == wanted
            })
        })
        .map(|entry| entry.id.as_str())
        .collect();
    match matches.as_slice() {
        [id] => Ok((*id).to_string()),
        [] => bail!("no external plugin in plugins.toml uses {}", binary.display()),
        _ => bail!("multiple plugins in plugins.toml use {}", binary.display()),
    }
}

/// 请求热替换并等待对应插件的结构化回执。
pub async fn run(
    config_path: &Path,
    id: Option<&str>,
    binary: Option<&Path>,
    drain_timeout_secs: u64,
) -> Result<()> {
    let id = resolve_plugin_id(config_path, id, binary)?;
    let config = CoreConfig::load(config_path)?;
    let url = management_url(&config.core.management_address);
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("invalid management URL {url}"))?;
    if !config.core.management_access_token.is_empty() {
        let value = format!("Bearer {}", config.core.management_access_token)
            .parse()
            .context("management access token is not a valid HTTP header")?;
        request.headers_mut().insert(AUTHORIZATION, value);
    }

    let operation = async {
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .with_context(|| format!("connect management WS {url}"))?;
        let command = serde_json::to_string(&BackendCommand::RestartPlugin { id: id.clone() })
            .context("serialize RestartPlugin")?;
        socket
            .send(Message::Text(command.into()))
            .await
            .context("send RestartPlugin")?;

        while let Some(frame) = socket.next().await {
            let frame = frame.context("read management WS")?;
            let Message::Text(text) = frame else {
                continue;
            };
            let Ok(event) = serde_json::from_str::<BackendEvent>(text.as_str()) else {
                continue;
            };
            match event {
                BackendEvent::PluginOpResult {
                    id: result_id,
                    op,
                    ok,
                    message,
                } if result_id == id && op == "restart" => {
                    if ok {
                        println!("{message}");
                        return Ok(());
                    }
                    bail!(message);
                }
                _ => {}
            }
        }
        bail!("management WS closed before plugin upgrade completed")
    };

    // 旧实例 drain 之外再给连接、握手和回执留固定余量。
    tokio::time::timeout(
        Duration::from_secs(drain_timeout_secs.saturating_add(15)),
        operation,
    )
    .await
    .context("timed out waiting for plugin upgrade acknowledgement")?
}

#[cfg(test)]
mod tests {
    use super::management_url;

    #[test]
    fn wildcard_management_addresses_connect_via_loopback() {
        assert_eq!(management_url("0.0.0.0:3132"), "ws://127.0.0.1:3132");
        assert_eq!(management_url("[::]:3132"), "ws://[::1]:3132");
        assert_eq!(management_url("127.0.0.1:3132"), "ws://127.0.0.1:3132");
    }

    #[test]
    fn explicit_websocket_scheme_is_preserved() {
        assert_eq!(management_url("ws://localhost:3132"), "ws://localhost:3132");
    }
}
