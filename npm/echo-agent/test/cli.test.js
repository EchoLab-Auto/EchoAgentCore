//! CLI 端到端测试：以 PATH 注入的假 `docker` 脚本替换真实 Docker，
//! 验证参数拼装、初始化、幂等与错误路径。
//!
//! 为什么用假 docker：CI/开发机未必有 Docker，但 CLI 的行为（调什么命令、
//! 传什么参数、打印什么提示）必须可测。

import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { beforeEach, describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

const CLI = fileURLToPath(new URL('../bin/echo-agent.js', import.meta.url));

let sandbox;
let logFile;

/** 每个用例一个沙箱：假 docker 的 bin 目录 + 调用日志文件 + 部署目录。 */
function setup() {
  sandbox = fs.mkdtempSync(path.join(os.tmpdir(), 'echo-agent-cli-'));
  logFile = path.join(sandbox, 'docker-calls.log');
  fs.writeFileSync(logFile, '');
}

/** 写一个假 docker：把调用记到 $FAKE_DOCKER_LOG；failInfo=true 时 `docker info` 失败。 */
function writeFakeDocker({ failInfo = false } = {}) {
  const bin = path.join(sandbox, 'bin');
  fs.mkdirSync(bin, { recursive: true });
  const script = [
    '#!/bin/sh',
    'printf "docker %s\\n" "$*" >> "$FAKE_DOCKER_LOG"',
    failInfo ? 'if [ "$1" = "info" ]; then exit 1; fi' : '',
    'exit 0',
    '',
  ].join('\n');
  fs.writeFileSync(path.join(bin, 'docker'), script, { mode: 0o755 });
  return bin;
}

/** 运行 CLI（返回退出码与输出）。 */
function runCli(args, { fakeBin, extraEnv = {} } = {}) {
  const env = {
    ...process.env,
    FAKE_DOCKER_LOG: logFile,
    ...extraEnv,
  };
  if (fakeBin) {
    env.PATH = `${fakeBin}:${process.env.PATH}`;
  }
  return new Promise((resolve) => {
    execFile(process.execPath, [CLI, ...args], { env }, (error, stdout, stderr) => {
      resolve({ status: error?.code ?? 0, stdout, stderr });
    });
  });
}

const dockerCalls = () =>
  fs
    .readFileSync(logFile, 'utf8')
    .split('\n')
    .filter(Boolean);

const deployDir = () => path.join(sandbox, 'deploy');

describe('echo-agent CLI', () => {
  beforeEach(setup);

  it('init：生成 compose / 配置 / .env 与数据目录', async () => {
    const fakeBin = writeFakeDocker();
    const result = await runCli(['init', '--dir', deployDir()], { fakeBin });
    assert.equal(result.status, 0, result.stderr);
    for (const file of [
      'docker-compose.yml',
      '.env',
      'config/core.toml',
      'config/panel.toml',
      'data',
    ]) {
      assert.ok(fs.existsSync(path.join(deployDir(), file)), `缺少 ${file}`);
    }
    assert.match(result.stdout, /部署目录/);
    assert.match(result.stdout, /api_key/);
  });

  it('init 幂等：已存在文件保留；--force 覆盖', async () => {
    const fakeBin = writeFakeDocker();
    await runCli(['init', '--dir', deployDir()], { fakeBin });
    const coreConfig = path.join(deployDir(), 'config/core.toml');
    fs.writeFileSync(coreConfig, '# 用户改过的内容\n');

    const second = await runCli(['init', '--dir', deployDir()], { fakeBin });
    assert.equal(second.status, 0);
    assert.match(second.stdout, /保留/);
    assert.equal(fs.readFileSync(coreConfig, 'utf8'), '# 用户改过的内容\n');

    const forced = await runCli(['init', '--dir', deployDir(), '--force'], { fakeBin });
    assert.equal(forced.status, 0);
    assert.match(fs.readFileSync(coreConfig, 'utf8'), /EchoAgentCore 容器模式配置/);
  });

  it('up：自动初始化 + compose up -d + 打印面板/扫码地址', async () => {
    const fakeBin = writeFakeDocker();
    const result = await runCli(['up', '--dir', deployDir()], { fakeBin });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /自动初始化/);
    const calls = dockerCalls();
    const expected =
      `compose --project-directory ${deployDir()} --file ${deployDir()}/docker-compose.yml up -d`;
    assert.ok(calls.includes(`docker ${expected}`), `实际调用：\n${calls.join('\n')}`);
    assert.match(result.stdout, /http:\/\/localhost:8080/);
    assert.match(result.stdout, /http:\/\/localhost:6099/);
  });

  it('down / restart / update / status / logs 的参数拼装', async () => {
    const fakeBin = writeFakeDocker();
    await runCli(['up', '--dir', deployDir()], { fakeBin });
    fs.writeFileSync(logFile, '');

    await runCli(['down', '--dir', deployDir()], { fakeBin });
    await runCli(['restart', '--dir', deployDir()], { fakeBin });
    await runCli(['update', '--dir', deployDir()], { fakeBin });
    await runCli(['status', '--dir', deployDir()], { fakeBin });
    await runCli(['logs', 'core', '-f', '--tail', '50', '--dir', deployDir()], { fakeBin });

    const calls = dockerCalls();
    assert.ok(calls.some((c) => c.endsWith(' down')), calls.join('\n'));
    assert.ok(calls.some((c) => c.endsWith(' restart')), calls.join('\n'));
    assert.ok(calls.some((c) => c.endsWith(' pull')), calls.join('\n'));
    assert.ok(calls.some((c) => c.endsWith(' ps')), calls.join('\n'));
    assert.ok(calls.some((c) => c.endsWith(' logs --tail 50 --follow core')), calls.join('\n'));
    // update = pull + up -d（顺序）
    const pullIndex = calls.findIndex((c) => c.endsWith(' pull'));
    const upAfter = calls.slice(pullIndex + 1).find((c) => c.endsWith(' up -d'));
    assert.ok(upAfter, 'update 应在 pull 后 up -d');
  });

  it('docker 缺失：up 给出可操作错误并以非零退出', async () => {
    // PATH 只剩一个空目录：node 用绝对路径调用，docker 必然找不到。
    const emptyBin = path.join(sandbox, 'empty-bin');
    fs.mkdirSync(emptyBin, { recursive: true });
    const result = await runCli(['up', '--dir', deployDir()], {
      extraEnv: { PATH: emptyBin },
    });
    assert.notEqual(result.status, 0);
    assert.match(result.stderr, /未找到 docker/);
    // 不因缺少 docker 就静默失败：不产生任何部署文件写入副作用
    assert.ok(!fs.existsSync(path.join(deployDir(), 'docker-compose.yml')));
  });

  it('doctor：环境正常 → 0；daemon 不可达 → 非零 + 明确指认', async () => {
    const healthy = writeFakeDocker();
    const ok = await runCli(['doctor', '--dir', deployDir()], { fakeBin: healthy });
    assert.equal(ok.status, 0, ok.stderr);
    assert.match(ok.stdout, /docker CLI/);

    const broken = writeFakeDocker({ failInfo: true });
    const bad = await runCli(['doctor', '--dir', deployDir()], { fakeBin: broken });
    assert.equal(bad.status, 1);
    assert.match(bad.stdout, /Docker daemon/);
    assert.match(bad.stdout, /存在阻断项/);
  });

  it('help 与 version', async () => {
    const help = await runCli(['--help']);
    assert.equal(help.status, 0);
    assert.match(help.stdout, /echo-agent <命令>/);
    assert.match(help.stdout, /http:\/\/localhost:8080/);

    const version = await runCli(['version']);
    assert.equal(version.status, 0);
    assert.match(version.stdout.trim(), /^\d+\.\d+\.\d+$/);
  });

  it('未知命令：非零退出并提示帮助', async () => {
    const result = await runCli(['frobnicate']);
    assert.equal(result.status, 1);
    assert.match(result.stderr, /未知命令/);
  });
});
