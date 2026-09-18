//! Configuration types for the QQ adapter.

use echo_adapter::filter::{
    AdminBypassFilter, AllowlistFilter, ContentLengthFilter, DenylistFilter, FilterPipeline,
    KeywordBlockFilter, RateLimitEntry, RateLimitFilter,
};
use serde::{Deserialize, Serialize};

/// Full QQ adapter configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QqAdapterConfig {
    /// Whether this adapter is enabled.
    pub enabled: bool,
    /// Reverse-WS server settings.
    pub server: QqServerConfig,
    /// Trigger gating (what messages auto-reply to).
    pub trigger: QqTriggerConfig,
    /// Owner QQ number for admin bypass.
    pub owner_qq: i64,
    /// Command prefix for built-in commands.
    pub command_prefix: String,
    /// Filter configuration.
    pub filter: QqFilterConfig,
    /// Gate mode configuration.
    #[serde(default)]
    pub gate: QqGateConfig,
    /// NapCat WebUI URL for API integration.
    /// Default `http://localhost:6099`.
    #[serde(default = "default_napcat_url")]
    pub napcat_webui_url: String,
    /// OneBot HTTP API URL used for management queries such as the full
    /// group/friend list. Default `http://localhost:3000`.
    #[serde(default = "default_onebot_url")]
    pub napcat_onebot_url: String,
    /// Host used to reach NapCat's reverse-WS port when auto-configuring it.
    /// Defaults to `host.docker.internal` (NapCat in Docker); set to
    /// `127.0.0.1` when running everything on one host.
    #[serde(default = "default_napcat_host")]
    pub napcat_host: String,
    /// Docker container name of NapCat. Used to copy files into the container
    /// (`docker cp`) when the Docker CLI is available.
    #[serde(default = "default_napcat_container")]
    pub napcat_container: String,
    /// Writable directory inside the NapCat container where files are copied
    /// to, and which is considered "already visible to NapCat".
    #[serde(default = "default_napcat_data_dir")]
    pub napcat_container_data_dir: String,
    /// Start the NapCat Docker container automatically when the QQ adapter
    /// starts. When false, NapCat is assumed to be managed externally.
    #[serde(default = "default_true")]
    pub napcat_auto_start: bool,
    /// Stop the NapCat Docker container automatically when the QQ adapter
    /// stops. Does not remove the container or its data volumes.
    ///
    /// 默认 **false**（2026-09-18 起）：Core 停机/重启不再联动停止 NapCat——
    /// 容器保持运行则 QQ 始终保持在线（NapCat 自身维持与 QQ 服务器的连接），
    /// Core 重启只断反向 WS、重启后自动重连，用户无感知；同时避免频繁容器
    /// 重启触发 QQ 安全策略导致快速登录凭证失效（"用户身份已失效"）。
    /// 需要联动停止时显式配置 `napcat_auto_stop = true`。
    #[serde(default)]
    pub napcat_auto_stop: bool,
    /// Docker Compose file used to start/stop the NapCat service. The file is
    /// relative to the Core working directory when no absolute path is given.
    #[serde(default = "default_napcat_compose_file")]
    pub napcat_compose_file: String,
    /// 接收文件（群文件上传 / 私聊文件）的行为配置。
    #[serde(default)]
    pub files: QqFilesConfig,
}

fn default_true() -> bool {
    true
}

fn default_napcat_url() -> String {
    "http://localhost:6099".into()
}

fn default_onebot_url() -> String {
    "http://localhost:3000".into()
}

fn default_napcat_host() -> String {
    "host.docker.internal".into()
}

fn default_napcat_container() -> String {
    "napcat".into()
}

fn default_napcat_data_dir() -> String {
    "/app/napcat/data".into()
}

fn default_napcat_compose_file() -> String {
    "docker-compose.yml".into()
}

impl Default for QqAdapterConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            server: QqServerConfig::default(),
            trigger: QqTriggerConfig::default(),
            owner_qq: 0,
            command_prefix: "/".into(),
            filter: QqFilterConfig::default(),
            gate: QqGateConfig::default(),
            napcat_webui_url: default_napcat_url(),
            napcat_onebot_url: default_onebot_url(),
            napcat_host: default_napcat_host(),
            napcat_container: default_napcat_container(),
            napcat_container_data_dir: default_napcat_data_dir(),
            napcat_auto_start: default_true(),
            files: QqFilesConfig::default(),
            napcat_auto_stop: default_true(),
            napcat_compose_file: default_napcat_compose_file(),
        }
    }
}

impl QqAdapterConfig {
    /// Build the filter pipeline from this config.
    pub fn build_filter_pipeline(&self) -> FilterPipeline {
        let mut pipeline = FilterPipeline::new();

        // Admin bypass filter (runs first).
        let admin_ids: Vec<String> = if self.owner_qq > 0 {
            vec![self.owner_qq.to_string()]
        } else {
            vec![]
        };
        let admin_filter = AdminBypassFilter::new(admin_ids);
        if admin_filter.is_active() {
            pipeline.push(Box::new(admin_filter));
        }

        // Allowlist.
        let allowlist = AllowlistFilter::new(
            self.filter
                .allowlist
                .user_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
            self.filter
                .allowlist
                .group_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
        );
        if allowlist.is_active() {
            pipeline.push(Box::new(allowlist));
        }

        // Denylist.
        let denylist = DenylistFilter::new(
            self.filter
                .denylist
                .user_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
            self.filter
                .denylist
                .group_ids
                .iter()
                .map(|id| id.to_string())
                .collect(),
        );
        if denylist.is_active() {
            pipeline.push(Box::new(denylist));
        }

        // Rate limit.
        let rate_limiter = RateLimitFilter::new(
            self.filter
                .rate_limit
                .per_user
                .as_ref()
                .map(|r| RateLimitEntry {
                    max_requests: r.max_requests,
                    window_seconds: r.window_seconds,
                }),
            self.filter
                .rate_limit
                .per_group
                .as_ref()
                .map(|r| RateLimitEntry {
                    max_requests: r.max_requests,
                    window_seconds: r.window_seconds,
                }),
            self.filter
                .rate_limit
                .global
                .as_ref()
                .map(|r| RateLimitEntry {
                    max_requests: r.max_requests,
                    window_seconds: r.window_seconds,
                }),
        );
        if rate_limiter.is_active() {
            pipeline.push(Box::new(rate_limiter));
        }

        // Keyword blocking.
        let keyword = KeywordBlockFilter::new(
            self.filter.keyword.block_keywords.clone(),
            &self.filter.keyword.block_regex,
        );
        if keyword.is_active() {
            pipeline.push(Box::new(keyword));
        }

        // Content length.
        if self.filter.content.max_length > 0 {
            pipeline.push(Box::new(ContentLengthFilter::new(
                self.filter.content.max_length,
            )));
        }

        pipeline
    }

    /// Build the filter pipeline respecting a gate mode (allowlist-only,
    /// denylist-only, or neither). The default [`build_filter_pipeline`]
    /// pushes both unconditionally.
    pub fn build_filter_pipeline_gated(&self, mode: &str) -> FilterPipeline {
        let mut pipeline = FilterPipeline::new();

        // Admin bypass.
        let admin_ids: Vec<String> = if self.owner_qq > 0 {
            vec![self.owner_qq.to_string()]
        } else {
            vec![]
        };
        let admin_filter = AdminBypassFilter::new(admin_ids);
        if admin_filter.is_active() {
            pipeline.push(Box::new(admin_filter));
        }

        // Allowlist / Denylist — mutually exclusive based on gate mode.
        match mode {
            "allowlist" => {
                let f = AllowlistFilter::new(
                    self.filter
                        .allowlist
                        .user_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect(),
                    self.filter
                        .allowlist
                        .group_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect(),
                );
                if f.is_active() {
                    pipeline.push(Box::new(f));
                }
            }
            "denylist" => {
                let f = DenylistFilter::new(
                    self.filter
                        .denylist
                        .user_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect(),
                    self.filter
                        .denylist
                        .group_ids
                        .iter()
                        .map(|id| id.to_string())
                        .collect(),
                );
                if f.is_active() {
                    pipeline.push(Box::new(f));
                }
            }
            _ => {} // none — no allowlist or denylist
        }

        // Rate limit, keyword, content length — shared.
        let rate_limiter = RateLimitFilter::new(
            self.filter
                .rate_limit
                .per_user
                .as_ref()
                .map(|r| RateLimitEntry {
                    max_requests: r.max_requests,
                    window_seconds: r.window_seconds,
                }),
            self.filter
                .rate_limit
                .per_group
                .as_ref()
                .map(|r| RateLimitEntry {
                    max_requests: r.max_requests,
                    window_seconds: r.window_seconds,
                }),
            self.filter
                .rate_limit
                .global
                .as_ref()
                .map(|r| RateLimitEntry {
                    max_requests: r.max_requests,
                    window_seconds: r.window_seconds,
                }),
        );
        if rate_limiter.is_active() {
            pipeline.push(Box::new(rate_limiter));
        }

        let keyword = KeywordBlockFilter::new(
            self.filter.keyword.block_keywords.clone(),
            &self.filter.keyword.block_regex,
        );
        if keyword.is_active() {
            pipeline.push(Box::new(keyword));
        }

        if self.filter.content.max_length > 0 {
            pipeline.push(Box::new(ContentLengthFilter::new(
                self.filter.content.max_length,
            )));
        }

        pipeline
    }
}

// ---------------------------------------------------------------------------
// Sub-configs
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QqServerConfig {
    pub bind_address: String,
    pub access_token: String,
    pub heartbeat_interval: u64,
}

impl Default for QqServerConfig {
    fn default() -> Self {
        Self {
            bind_address: "0.0.0.0:3131".into(),
            access_token: String::new(),
            heartbeat_interval: 30,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QqTriggerConfig {
    /// Auto-reply to private messages.
    pub dm_auto_reply: bool,
    /// Reply to group messages only when @-mentioned.
    pub group_at_reply: bool,
}

impl Default for QqTriggerConfig {
    fn default() -> Self {
        Self {
            dm_auto_reply: true,
            group_at_reply: true,
        }
    }
}

/// 接收文件的行为配置。
///
/// 群文件上传（`group_upload` 通知）与私聊文件（message 里的 `file` 段）
/// 在 NapCat 侧可各自独立开关；下载失败/超限时仍会把条目送达 agent
/// （`path = null` + `error` 说明），以免用户发了个文件却「石沉大海」。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QqFilesConfig {
    /// 接收文件的保存目录；空 = 默认数据目录
    /// （`~/.local/share/echo-agent-core/downloads`，HOME 不可用时为
    /// 工作目录下的 `downloads`）。
    pub dir: String,
    /// 单文件大小上限（MB）。超过则跳过下载（条目仍送达，带原因）。
    pub max_mb: u64,
    /// 是否接收群文件上传通知。
    pub accept_group_upload: bool,
    /// 是否接收私聊文件。
    pub accept_private_file: bool,
}

impl Default for QqFilesConfig {
    fn default() -> Self {
        Self {
            dir: String::new(),
            max_mb: 100,
            accept_group_upload: true,
            accept_private_file: true,
        }
    }
}

impl QqFilesConfig {
    /// 单文件大小上限（字节）；`max_mb` 为 0 时回退默认 100MB。
    pub fn max_bytes(&self) -> u64 {
        let mb = if self.max_mb == 0 { 100 } else { self.max_mb };
        mb.saturating_mul(1024 * 1024)
    }
}

// ---------------------------------------------------------------------------
// Filter config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct QqFilterConfig {
    pub allowlist: AllowlistConfig,
    pub denylist: DenylistConfig,
    pub rate_limit: RateLimitConfig,
    pub keyword: KeywordConfig,
    pub content: ContentConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct AllowlistConfig {
    pub user_ids: Vec<i64>,
    pub group_ids: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct DenylistConfig {
    pub user_ids: Vec<i64>,
    pub group_ids: Vec<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct RateLimitConfig {
    pub per_user: Option<RateLimitEntryConfig>,
    pub per_group: Option<RateLimitEntryConfig>,
    pub global: Option<RateLimitEntryConfig>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RateLimitEntryConfig {
    pub max_requests: u32,
    pub window_seconds: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct KeywordConfig {
    pub block_keywords: Vec<String>,
    pub block_regex: String,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(default)]
pub struct ContentConfig {
    pub max_length: usize,
}

/// Gate mode — persisted across restarts.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct QqGateConfig {
    /// "none" | "allowlist" | "denylist"
    pub mode: String,
}

impl Default for QqGateConfig {
    fn default() -> Self {
        Self {
            mode: "none".into(),
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_config_defaults_to_none() {
        let cfg = QqGateConfig::default();
        assert_eq!(cfg.mode, "none");
    }

    #[test]
    fn gate_config_deserializes_from_toml() {
        let toml = r#"
[adapters.qq.gate]
mode = "allowlist"
"#;
        #[derive(serde::Deserialize)]
        struct Wrap {
            #[serde(rename = "adapters")]
            a: AdaptersWrap,
        }
        #[derive(serde::Deserialize)]
        struct AdaptersWrap {
            qq: QqAdapterConfig,
        }
        let w: Wrap = toml::from_str(toml).unwrap();
        assert_eq!(w.a.qq.gate.mode, "allowlist");
    }

    #[test]
    fn allowlist_config_defaults_empty() {
        let cfg = AllowlistConfig::default();
        assert!(cfg.user_ids.is_empty());
        assert!(cfg.group_ids.is_empty());
    }

    #[test]
    fn filter_pipeline_empty_by_default() {
        let cfg = QqAdapterConfig::default();
        let pipeline = cfg.build_filter_pipeline();
        assert_eq!(pipeline.len(), 0); // No filters active when empty defaults
    }

    #[test]
    fn filter_pipeline_includes_admin_bypass() {
        let cfg = QqAdapterConfig {
            owner_qq: 12345,
            ..Default::default()
        };
        let pipeline = cfg.build_filter_pipeline();
        assert!(!pipeline.is_empty());
    }

    #[test]
    fn filter_pipeline_includes_allowlist_when_populated() {
        let mut cfg = QqAdapterConfig::default();
        cfg.filter.allowlist.user_ids = vec![111, 222];
        let pipeline = cfg.build_filter_pipeline();
        assert!(!pipeline.is_empty());
    }

    #[test]
    fn gated_pipeline_none_mode_has_no_lists() {
        let mut cfg = QqAdapterConfig::default();
        cfg.filter.allowlist.user_ids = vec![111];
        cfg.filter.denylist.user_ids = vec![222];
        // none: neither allowlist nor denylist is active.
        let pipeline = cfg.build_filter_pipeline_gated("none");
        assert_eq!(pipeline.len(), 0, "no filters for gate mode none");
    }

    #[test]
    fn gated_pipeline_allowlist_excludes_denylist() {
        let mut cfg = QqAdapterConfig::default();
        cfg.filter.allowlist.user_ids = vec![111];
        cfg.filter.denylist.user_ids = vec![222];
        let pipeline = cfg.build_filter_pipeline_gated("allowlist");
        // Allowlist active; denylist must NOT be in the pipeline.
        assert!(!pipeline.is_empty());
        let names = pipeline.filter_names();
        assert!(names.iter().any(|n| n.contains("allowlist")));
        assert!(
            !names.iter().any(|n| n.contains("denylist")),
            "denylist excluded in allowlist mode"
        );
    }

    #[test]
    fn gated_pipeline_denylist_excludes_allowlist() {
        let mut cfg = QqAdapterConfig::default();
        cfg.filter.allowlist.user_ids = vec![111];
        cfg.filter.denylist.user_ids = vec![222];
        let pipeline = cfg.build_filter_pipeline_gated("denylist");
        let names = pipeline.filter_names();
        assert!(names.iter().any(|n| n.contains("denylist")));
        assert!(
            !names.iter().any(|n| n.contains("allowlist")),
            "allowlist excluded in denylist mode"
        );
    }

    #[test]
    fn gated_pipeline_keeps_shared_filters() {
        let mut cfg = QqAdapterConfig::default();
        cfg.filter.content.max_length = 500;
        cfg.owner_qq = 12345;
        let pipeline = cfg.build_filter_pipeline_gated("none");
        // Admin bypass + content length still apply.
        assert!(pipeline.len() >= 2, "shared filters must survive gating");
    }

    #[test]
    fn gated_pipeline_unknown_mode_falls_back_to_none() {
        let mut cfg = QqAdapterConfig::default();
        cfg.filter.allowlist.user_ids = vec![111];
        let pipeline = cfg.build_filter_pipeline_gated("bogus-mode");
        assert_eq!(pipeline.len(), 0);
    }
}
