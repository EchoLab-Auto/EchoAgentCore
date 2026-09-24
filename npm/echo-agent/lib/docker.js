//! docker / docker compose 调用的薄封装：可用性探测、参数拼装、命令执行。
//!
//! 所有对 Docker 的调用集中在这里，便于测试（测试用 PATH 上的假 docker
//! 脚本替换真实实现）与统一错误提示。

import { spawn, spawnSync } from 'node:child_process';
import path from 'node:path';

/** 命令是否可执行（`<cmd> <probe args>` 返回 0）。 */
export function hasCommand(cmd, args) {
  try {
    const result = spawnSync(cmd, args, { stdio: 'ignore' });
    return result.status === 0;
  } catch {
    return false;
  }
}

/** `docker` CLI 是否存在。 */
export function dockerAvailable() {
  return hasCommand('docker', ['--version']);
}

/** `docker compose` 插件是否可用。 */
export function composeAvailable() {
  return hasCommand('docker', ['compose', 'version']);
}

/**
 * 拼装 compose 调用参数。
 *
 * 显式 `--project-directory`：保证 `.env` 与相对 bind mount 都相对部署目录
 * 解析，而不是调用者的当前目录。
 */
export function composeArgs(dir, args) {
  return [
    'compose',
    '--project-directory',
    dir,
    '--file',
    path.join(dir, 'docker-compose.yml'),
    ...args,
  ];
}

/** 执行命令并继承 stdio（交互/流式输出；返回退出码）。 */
export function run(cmd, args, { cwd, env } = {}) {
  return new Promise((resolve) => {
    const child = spawn(cmd, args, {
      stdio: 'inherit',
      cwd,
      env: { ...process.env, ...env },
    });
    child.on('error', (error) => resolve({ status: 127, error }));
    child.on('close', (status) => resolve({ status: status ?? 1 }));
  });
}

/** 执行命令并捕获输出（用于 status / doctor 的判断）。 */
export function runCapture(cmd, args, { cwd, env } = {}) {
  return new Promise((resolve) => {
    const child = spawn(cmd, args, {
      cwd,
      env: { ...process.env, ...env },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => (stdout += chunk));
    child.stderr.on('data', (chunk) => (stderr += chunk));
    child.on('error', (error) => resolve({ status: 127, stdout, stderr, error }));
    child.on('close', (status) => resolve({ status: status ?? 1, stdout, stderr }));
  });
}

/** 执行 `docker compose <args>`（继承 stdio）。 */
export function runCompose(dir, args, options) {
  return run('docker', composeArgs(dir, args), options);
}

/** 执行 `docker compose <args>` 并捕获输出。 */
export function captureCompose(dir, args, options) {
  return runCapture('docker', composeArgs(dir, args), options);
}
