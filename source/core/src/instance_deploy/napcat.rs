//! **QQ / NapCat 容器部署知识**——本仓库里唯一还在描述「怎么把某个平台跑起来」
//! 的地方。
//!
//! 为什么单独成文件：实例编排（persona 归属、端口分配、配置合并）是通用机制，
//! 而「NapCat 需要哪三个端口、镜像叫什么、数据卷挂哪里」是平台部署知识。
//! 两者混在一个 622 行的文件里，导致「加一个平台」必须读懂全部 QQ 细节。
//!
//! 迁移路径（后续阶段，非本次范围）：插件在自己的 `InstanceContribution` 里声明
//! 部署规格（镜像 / 端口映射 / 卷），宿主按声明渲染——那时本文件整体删除。当前
//! QQ 是唯一内置插件实例，先把它隔离到单一出口，迁移时只需搬这一个文件。

use std::path::{Path, PathBuf};

use crate::config::QqPorts;

/// 生成实例的 compose 文件内容（独立容器名 / 端口 / 数据卷）。
pub fn render_compose(id: &str, ports: &QqPorts) -> String {
    format!(
        r#"# 由 EchoAgentCore 自动生成（QQ 实例 `{id}`）——请勿手工编辑。
# 端口映射由 [instances.qq.instances.{id}.ports] 决定。
services:
  napcat:
    image: mlikiowa/napcat-docker:latest
    container_name: echo-napcat-{id}
    restart: unless-stopped
    ports:
      - "{webui}:6099"      # WebUI（扫码登录）
      - "{onebot}:3000"     # HTTP API
    environment:
      - NAPCAT_UID=0
      - NAPCAT_GID=0
    extra_hosts:
      - "host.docker.internal:host-gateway"
    volumes:
      - echo-napcat-{id}-data:/app/napcat/data
      - echo-napcat-{id}-config:/app/napcat/config

volumes:
  echo-napcat-{id}-data:
  echo-napcat-{id}-config:
"#,
        id = id,
        webui = ports.webui,
        onebot = ports.onebot_http,
    )
}

/// 写实例 compose 文件（幂等；内容变化才写）。
pub fn write_compose(data_dir: &Path, id: &str, ports: &QqPorts) -> Result<PathBuf, String> {
    let dir = data_dir.join("napcat").join(id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let path = dir.join("docker-compose.yml");
    let content = render_compose(id, ports);
    let unchanged = std::fs::read_to_string(&path)
        .map(|old| old == content)
        .unwrap_or(false);
    if !unchanged {
        std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
    }
    Ok(path)
}

/// 自动生成的 compose 路径（口径与 [`write_compose`] 一致）。
pub fn auto_compose_path(data_dir: Option<&Path>, id: &str) -> Option<String> {
    data_dir.map(|dir| {
        dir.join("napcat")
            .join(id)
            .join("docker-compose.yml")
            .to_string_lossy()
            .to_string()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_carries_instance_scoped_names_and_ports() {
        let ports = QqPorts {
            reverse_ws: 3141,
            onebot_http: 3041,
            webui: 6141,
        };
        let text = render_compose("alix", &ports);
        assert!(text.contains("container_name: echo-napcat-alix"));
        assert!(text.contains("\"6141:6099\""));
        assert!(text.contains("\"3041:3000\""));
        assert!(text.contains("echo-napcat-alix-data"));
        // 反向 WS 由 Core 自己绑定，不进容器端口映射
        assert!(!text.contains("3141:"));
    }

    #[test]
    fn write_compose_is_idempotent() {
        let dir =
            std::env::temp_dir().join(format!("echo-napcat-deploy-test-{}", std::process::id()));
        let ports = QqPorts {
            reverse_ws: 3131,
            onebot_http: 3000,
            webui: 6099,
        };
        let path = write_compose(&dir, "qq", &ports).expect("write ok");
        let first = std::fs::read_to_string(&path).unwrap();
        let path2 = write_compose(&dir, "qq", &ports).expect("rewrite ok");
        assert_eq!(path, path2);
        assert_eq!(std::fs::read_to_string(&path2).unwrap(), first);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_path_matches_write_location() {
        let dir = std::path::Path::new("/tmp/x");
        assert_eq!(
            auto_compose_path(Some(dir), "alix").unwrap(),
            "/tmp/x/napcat/alix/docker-compose.yml"
        );
        assert!(auto_compose_path(None, "alix").is_none());
    }
}
