//! 命令实现：init / up / down / restart / update / logs / status / doctor。
//!
//! 设计约定：
//! - 一切以"部署目录"为中心（`init` 生成、`up` 使用的同一份 compose）；
//! - 对 Docker 的调用全部经 `lib/docker.js`（测试可注入假 docker）；
//! - 用户可见文案统一 `[echo-agent]` 前缀，关键下一步（面板地址 / 扫码地址）
//!   在 `up` 结束时打印。

import fs from 'node:fs';
import net from 'node:net';
import path from 'node:path';

import {
  captureCompose,
  composeAvailable,
  dockerAvailable,
  runCapture,
  runCompose,
} from './docker.js';
import { deployPaths, readTemplate } from './paths.js';

const PANEL_URL = 'http://localhost:8080';
const NAPCAT_URL = 'http://localhost:6099';

function log(line = '') {
  console.log(line);
}

/** 生成部署文件（幂等：已存在默认保留，`--force` 覆盖）。 */
export function init({ dir, force = false }) {
  const paths = deployPaths(dir);
  fs.mkdirSync(paths.configDir, { recursive: true });
  fs.mkdirSync(paths.dataDir, { recursive: true });

  const files = [
    ['docker-compose.yml', paths.composeFile, 'docker-compose.yml'],
    ['core.toml', paths.coreConfig, 'config/core.toml'],
    ['panel.toml', paths.panelConfig, 'config/panel.toml'],
    ['env', paths.envFile, '.env'],
  ];

  log(`[echo-agent] 部署目录：${paths.dir}`);
  for (const [template, target, label] of files) {
    if (fs.existsSync(target) && !force) {
      log(`  保留  ${label}（已存在；--force 可覆盖）`);
      continue;
    }
    fs.writeFileSync(target, readTemplate(template));
    log(`  写入  ${label}`);
  }

  log();
  log('下一步：');
  log(`  1. 编辑 ${paths.coreConfig}`);
  log('     填入 LLM API Key（[agent] api_key，或改用 DEEPSEEK_API_KEY 环境变量）');
  log('  2. echo-agent up          # 拉取镜像并启动 core + panel + napcat');
  log(`  3. 打开面板 ${PANEL_URL}（首次登录 QQ 用 ${NAPCAT_URL} 扫码）`);
  return 0;
}

function ensureDocker() {
  if (!dockerAvailable()) {
    throw new Error(
      '未找到 docker 命令。请先安装 Docker（Docker Desktop 或 Docker Engine），并确认 docker 在 PATH 中。',
    );
  }
  if (!composeAvailable()) {
    throw new Error(
      'docker compose 插件不可用。Docker Desktop 自带；Linux 请安装 docker-compose-plugin。',
    );
  }
}

/** 未初始化时自动初始化（up/restart 等命令的便利路径）。 */
function ensureInitialized(dir) {
  const paths = deployPaths(dir);
  if (fs.existsSync(paths.composeFile)) return false;
  log('[echo-agent] 未找到部署文件，自动初始化…');
  log();
  init({ dir });
  log();
  return true;
}

/** 启动（未初始化则先初始化）。 */
export async function up({ dir }) {
  ensureDocker();
  ensureInitialized(dir);
  const { status } = await runCompose(dir, ['up', '-d']);
  if (status !== 0) return status;
  log();
  log('[echo-agent] 已启动：');
  log(`  面板      ${PANEL_URL}`);
  log(`  QQ 登录   ${NAPCAT_URL}（首次扫码；也可在面板 QQ 页获取登录二维码）`);
  log('  提示      修改 config/*.toml 后执行 `echo-agent restart` 生效');
  return 0;
}

/** 停止并移除容器（保留配置与数据）。 */
export async function down({ dir }) {
  ensureDocker();
  ensureInitialized(dir);
  return (await runCompose(dir, ['down'])).status;
}

/** 重启容器（配置修改后生效）。 */
export async function restart({ dir }) {
  ensureDocker();
  ensureInitialized(dir);
  return (await runCompose(dir, ['restart'])).status;
}

/** 拉取最新镜像并重建（滚动升级）。 */
export async function update({ dir }) {
  ensureDocker();
  ensureInitialized(dir);
  const pull = await runCompose(dir, ['pull']);
  if (pull.status !== 0) return pull.status;
  return (await runCompose(dir, ['up', '-d'])).status;
}

/** 查看日志（默认 tail 200，`-f` 跟随）。 */
export async function logs({ dir, service, follow = false, tail = 200 }) {
  ensureDocker();
  ensureInitialized(dir);
  const args = ['logs', '--tail', String(tail)];
  if (follow) args.push('--follow');
  if (service) args.push(service);
  return (await runCompose(dir, args)).status;
}

/** 容器状态（compose ps）。 */
export async function status({ dir }) {
  ensureDocker();
  ensureInitialized(dir);
  const result = await captureCompose(dir, ['ps']);
  if (result.status !== 0) {
    process.stderr.write(result.stderr);
    return result.status;
  }
  log(result.stdout.trimEnd());
  log();
  log(`  面板 ${PANEL_URL} · QQ 登录 ${NAPCAT_URL}`);
  return 0;
}

/** 端口占用探测（连接成功 = 被占用）。 */
function portOpen(port, timeoutMs = 500) {
  return new Promise((resolve) => {
    const socket = net.connect({ port, host: '127.0.0.1' });
    const done = (open) => {
      socket.destroy();
      resolve(open);
    };
    socket.setTimeout(timeoutMs);
    socket.once('connect', () => done(true));
    socket.once('timeout', () => done(false));
    socket.once('error', () => done(false));
  });
}

/** 已存在的同名容器（compose 会因 container_name 冲突启动失败）。 */
async function existingContainers(names) {
  const found = [];
  for (const name of names) {
    const result = await runCapture('docker', [
      'ps',
      '-a',
      '--filter',
      `name=^${name}$`,
      '--format',
      '{{.Names}}',
    ]);
    if (result.status === 0 && result.stdout.trim() === name) found.push(name);
  }
  return found;
}

/** 环境自检：docker / daemon / compose / 端口 / 容器名冲突 / 部署目录。 */
export async function doctor({ dir }) {
  log('[echo-agent] doctor：环境检查');
  let hardFailure = false;

  const checks = [];

  // docker CLI
  if (dockerAvailable()) {
    const version = await runCapture('docker', ['--version']);
    checks.push(['ok', 'docker CLI', version.stdout.trim()]);
  } else {
    checks.push(['fail', 'docker CLI', '未找到 docker 命令（请安装 Docker Desktop / Engine）']);
    hardFailure = true;
  }

  // compose 插件
  if (dockerAvailable()) {
    if (composeAvailable()) {
      const version = await runCapture('docker', ['compose', 'version']);
      checks.push(['ok', 'docker compose', version.stdout.trim()]);
    } else {
      checks.push(['fail', 'docker compose', '插件不可用（Linux 需 docker-compose-plugin）']);
      hardFailure = true;
    }
  }

  // daemon
  if (dockerAvailable()) {
    const info = await runCapture('docker', ['info']);
    if (info.status === 0) {
      checks.push(['ok', 'Docker daemon', '运行中']);
    } else {
      checks.push(['fail', 'Docker daemon', '不可达（守护进程未启动或当前用户无权限）']);
      hardFailure = true;
    }
  }

  // 部署目录
  const paths = deployPaths(dir);
  if (fs.existsSync(paths.composeFile)) {
    checks.push(['ok', '部署目录', `${paths.dir}（已初始化）`]);
  } else {
    checks.push(['warn', '部署目录', `${paths.dir}（尚未初始化，运行 echo-agent init）`]);
  }

  // 端口
  for (const [port, what] of [
    [8080, '面板'],
    [6099, 'NapCat WebUI'],
  ]) {
    if (await portOpen(port)) {
      checks.push(['warn', `端口 ${port}`, `已被占用（${what}；若为旧部署请先停止）`]);
    } else {
      checks.push(['ok', `端口 ${port}`, '空闲']);
    }
  }

  // 容器名冲突
  if (dockerAvailable()) {
    const conflicts = await existingContainers(['napcat', 'echo-agent-core', 'echo-agent-panel']);
    if (conflicts.length) {
      checks.push([
        'warn',
        '容器名冲突',
        `${conflicts.join(', ')} 已存在（来自旧部署？compose up 会报名字冲突，先停止旧容器）`,
      ]);
    } else {
      checks.push(['ok', '容器名', '无冲突']);
    }
  }

  log();
  let warnings = 0;
  for (const [level, name, detail] of checks) {
    const mark = level === 'ok' ? '✓' : level === 'warn' ? '⚠' : '✗';
    if (level === 'warn') warnings += 1;
    log(`  ${mark} ${name.padEnd(16)} ${detail}`);
  }
  log();
  if (hardFailure) {
    log('存在阻断项（✗）：请先解决后再运行 echo-agent up。');
    return 1;
  }
  if (warnings > 0) {
    log(`检查完成（${warnings} 项警告）：通常可继续，但端口/容器名冲突会让 up 失败——`);
    log('若本机已有旧部署（宿主安装或旧容器），先停掉它再 up。');
    return 0;
  }
  log('检查通过，可运行 echo-agent up。');
  return 0;
}
