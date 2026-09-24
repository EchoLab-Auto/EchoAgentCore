//! 部署目录与模板文件的定位。
//!
//! 部署目录（默认 `~/.echo-agent`，可用 `--dir` / `$ECHO_AGENT_HOME` 覆盖）
//! 的结构：
//!
//! ```text
//! ~/.echo-agent/
//! ├── docker-compose.yml   # core + panel + napcat
//! ├── .env                 # 镜像引用（可切换本地构建镜像）
//! ├── config/
//! │   ├── core.toml        # Core 配置（LLM key、QQ 适配器…）
//! │   └── panel.toml       # Panel 配置（监听地址 / Core 地址 / 媒体目录）
//! └── data/                # 持久数据：媒体库、下载、NapCat 编排（容器 /data）
//! ```

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

/** 模板目录（随包分发）。 */
export function templatesDir() {
  return fileURLToPath(new URL('../templates/', import.meta.url));
}

/** 读取一个模板文件。 */
export function readTemplate(name) {
  return fs.readFileSync(path.join(templatesDir(), name), 'utf8');
}

/** 解析部署目录：显式 --dir > $ECHO_AGENT_HOME > ~/.echo-agent。 */
export function resolveDeployDir(explicit) {
  if (explicit) return path.resolve(explicit);
  const fromEnv = process.env.ECHO_AGENT_HOME;
  if (fromEnv && fromEnv.trim()) return path.resolve(fromEnv.trim());
  return path.join(os.homedir(), '.echo-agent');
}

/** 部署目录内的各路径。 */
export function deployPaths(dir) {
  return {
    dir,
    composeFile: path.join(dir, 'docker-compose.yml'),
    envFile: path.join(dir, '.env'),
    configDir: path.join(dir, 'config'),
    coreConfig: path.join(dir, 'config', 'core.toml'),
    panelConfig: path.join(dir, 'config', 'panel.toml'),
    dataDir: path.join(dir, 'data'),
  };
}

/** 包版本（`version` 命令用）。 */
export function packageVersion() {
  const raw = fs.readFileSync(new URL('../package.json', import.meta.url), 'utf8');
  return JSON.parse(raw).version;
}
