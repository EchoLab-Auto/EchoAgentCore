//! # echo-plugin-panel — 可选的进程内 Web Panel
//!
//! 本插件只做 **Core 插件生命周期 ↔ Panel 运行库** 的适配：HTTP/WS 路由、
//! 前端、上游与 SSH 隧道仍由 Panel 仓的 `echo-web-server` 拥有。独立
//! `echo-agent-panel` 外壳与本插件使用同一个运行库，避免两份实现漂移。
//!
//! ## 启用
//!
//! 编译进 Core 不等于开启服务。必须显式在 `plugins.toml` 中启用；缺省条目
//! 不挂载、不监听端口。因此现有“独立 Panel + Core”部署不变。
//!
//! ```toml
//! [[plugin]]
//! id = "echo-agent.panel.web"
//! kind = "builtin"
//! name = "Web Panel"
//! enabled = true
//!
//! [plugin.config]
//! bind = "127.0.0.1:8080"
//! static_dir = "/absolute/path/to/web/dist"
//! # token 请放 secret_file；插件配置会出现在诊断/清单面，不能放凭证。
//! access_token_secret_file = "/home/me/.config/echo-agent-panel/token"
//! # config_file 指向独立 Panel 的 TOML；可选，用于复用多上游与鉴权配置。
//! # config_file = "/home/me/.config/echo-agent-panel/panel.toml"
//! ```
//!
//! `config_file` 中的 bind/static_dir 可被这里显式覆盖。未指定 config_file
//! 时只有单上游直连配置；多上游/SSH/联邦发现应指向现有 Panel TOML，复用
//! 其运行期配置写回，而不是复制到插件配置中。
//!
//! 本机第一版保留 Panel 的 WS 中继，不修改 management 协议。是否改成宿主
//! 直接调用是后续优化，不与“进程内可启停”混在一起。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use echo_plugin::{BuiltinPlugin, Plugin, PluginManifest};
use echo_web_server::runtime::{PanelRuntime, PanelRuntimeConfig};

pub const PLUGIN_ID: &str = "echo-agent.panel.web";

#[derive(Debug, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Config {
    bind: String,
    static_dir: PathBuf,
    core: String,
    core_access_token: String,
    core_access_token_secret_file: Option<PathBuf>,
    access_token: String,
    access_token_secret_file: Option<PathBuf>,
    config_file: Option<PathBuf>,
    media_dir: PathBuf,
    media_cache_dir: PathBuf,
    allow_remote: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            static_dir: PathBuf::new(),
            core: "ws://127.0.0.1:3132".into(),
            core_access_token: String::new(),
            core_access_token_secret_file: None,
            access_token: String::new(),
            access_token_secret_file: None,
            config_file: None,
            media_dir: PathBuf::new(),
            media_cache_dir: PathBuf::new(),
            allow_remote: false,
        }
    }
}

fn secret(path: Option<&Path>) -> anyhow::Result<String> {
    let Some(path) = path else {
        return Ok(String::new());
    };
    let expanded = if path == Path::new("~") || path.starts_with("~/") {
        let home = std::env::var_os("HOME")
            .ok_or_else(|| anyhow::anyhow!("HOME unset for secret_file"))?;
        std::path::PathBuf::from(home).join(path.strip_prefix("~/").unwrap_or(Path::new("")))
    } else {
        path.to_path_buf()
    };
    Ok(std::fs::read_to_string(&expanded)
        .map_err(|e| anyhow::anyhow!("read secret_file {}: {e}", expanded.display()))?
        .trim()
        .to_owned())
}

fn runtime_config(ctx: &echo_plugin::MountContext) -> anyhow::Result<PanelRuntimeConfig> {
    let cfg: Config = if ctx.config().is_null() {
        Config::default()
    } else {
        serde_json::from_value(ctx.config().clone())
            .map_err(|e| anyhow::anyhow!("invalid {PLUGIN_ID} config: {e}"))?
    };
    let (bind, static_dir, core, core_token, panel_token, media_dir, media_cache_dir) =
        if let Some(path) = &cfg.config_file {
            let panel = echo_web_server::config::PanelConfig::load(path)?;
            let default_media_dir = panel.server.effective_media_dir();
            let default_media_cache_dir = panel.server.effective_media_cache_dir();
            let core = if cfg.core == Config::default().core {
                panel.core.connect_url
            } else {
                cfg.core
            };
            let token = if !cfg.core_access_token.is_empty() {
                cfg.core_access_token
            } else {
                panel.core.access_token
            };
            (
                if cfg.bind == Config::default().bind {
                    panel.server.bind_address
                } else {
                    cfg.bind
                },
                if cfg.static_dir.as_os_str().is_empty() {
                    PathBuf::from(panel.server.static_dir)
                } else {
                    cfg.static_dir
                },
                core,
                token,
                if !cfg.access_token.is_empty() {
                    cfg.access_token
                } else {
                    panel.server.access_token
                },
                if cfg.media_dir.as_os_str().is_empty() {
                    default_media_dir
                } else {
                    cfg.media_dir
                },
                if cfg.media_cache_dir.as_os_str().is_empty() {
                    default_media_cache_dir
                } else {
                    cfg.media_cache_dir
                },
            )
        } else {
            (
                cfg.bind,
                cfg.static_dir,
                cfg.core,
                cfg.core_access_token,
                cfg.access_token,
                cfg.media_dir,
                cfg.media_cache_dir,
            )
        };
    let core_access_token = if !core_token.is_empty() {
        core_token
    } else {
        secret(cfg.core_access_token_secret_file.as_deref())?
    };
    let access_token = if !panel_token.is_empty() {
        panel_token
    } else {
        secret(cfg.access_token_secret_file.as_deref())?
    };
    if !cfg.allow_remote
        && !(bind.starts_with("127.0.0.1:")
            || bind.starts_with("localhost:")
            || bind.starts_with("[::1]:"))
    {
        anyhow::bail!("embedded Panel bind {bind:?} is not loopback; configure access_token_secret_file and allow_remote=true to expose it");
    }
    if access_token.is_empty() && cfg.allow_remote {
        anyhow::bail!("embedded Panel refuses remote bind without access_token_secret_file");
    }
    if static_dir.as_os_str().is_empty() {
        anyhow::bail!("embedded Panel requires static_dir (the built web/dist directory)");
    }
    Ok(PanelRuntimeConfig {
        config_file: cfg.config_file,
        bind,
        static_dir,
        core,
        core_access_token,
        access_token,
        media_dir,
        media_cache_dir,
    })
}

/// 构造插件：空挂载只在配置启用后发生，真正资源归 `RunningPanel` 所有。
pub fn plugin(version: &str) -> Arc<dyn Plugin> {
    let manifest = PluginManifest::builtin(
        PLUGIN_ID,
        "Web Panel",
        version,
        "panel",
        "panel",
        "可选的进程内 Web Panel：HTTP/WS、前端、多上游；独立 Panel 外壳继续可用",
    );
    Arc::new(BuiltinPlugin::new(manifest, |ctx| {
        let config =
            runtime_config(ctx).map_err(|e| echo_plugin::PluginMountError::Mount(e.to_string()))?;
        let running = PanelRuntime::start(config).map_err(|e| {
            echo_plugin::PluginMountError::Mount(format!("start embedded Panel: {e}"))
        })?;
        Ok(vec![echo_context::Disposer::from_fn(move || {
            running.shutdown();
        })])
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_is_a_process_scoped_builtin() {
        let plugin = plugin("0.0.0-test");
        assert_eq!(plugin.manifest().id, PLUGIN_ID);
        assert_eq!(plugin.manifest().kind, "panel");
        assert!(plugin.manifest().builtin);
    }

    #[test]
    fn empty_config_does_not_bind_or_require_panel_files() {
        let ctx = echo_plugin::MountContext::default();
        let error = runtime_config(&ctx).unwrap_err().to_string();
        assert!(error.contains("static_dir"), "{error}");
    }

    #[test]
    fn remote_bind_is_fail_closed_without_explicit_authentication() {
        let ctx = echo_plugin::MountContext::default().with_config(serde_json::json!({
            "bind": "0.0.0.0:18080",
            "static_dir": "/tmp/panel-dist",
        }));
        let error = runtime_config(&ctx).unwrap_err().to_string();
        assert!(error.contains("allow_remote=true"), "{error}");
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    /// 真正的插件 mount → HTTP 请求 → unmount → 端口重新可绑。
    #[tokio::test(flavor = "current_thread")]
    async fn mount_serves_frontend_and_unmount_releases_port() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir = std::env::temp_dir().join(format!("embedded-panel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "embedded-panel-ok").unwrap();
        let plugin = plugin("test");
        let disposers = plugin
            .mount(
                &echo_plugin::MountContext::default().with_config(serde_json::json!({
                    "bind": format!("127.0.0.1:{port}"),
                    "static_dir": dir,
                    "core": "ws://127.0.0.1:9"
                })),
            )
            .expect("mount");
        let response = reqwest::get(format!("http://127.0.0.1:{port}/"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(response.contains("embedded-panel-ok"), "{response}");
        for disposer in disposers {
            disposer.dispose();
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("unmount releases listener");
        std::fs::remove_dir_all(dir).ok();
    }
}
