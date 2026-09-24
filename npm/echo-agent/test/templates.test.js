//! 模板契约与一致性测试。
//!
//! 关键不变量（改模板时最容易漏掉的跨文件耦合）：
//! - compose 注入的 `ECHO_MEDIA_DIR` == panel.toml 的 `media_dir`（否则图片 404）；
//! - compose 的 NapCat 容器名 == core.toml 的 `napcat_container`；
//! - 容器互访一律用 compose 服务名（core / napcat），不得出现 localhost；
//! - compose 语法有效（有 docker compose 时实际校验）。

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

const templates = fileURLToPath(new URL('../templates/', import.meta.url));

const compose = fs.readFileSync(path.join(templates, 'docker-compose.yml'), 'utf8');
const coreToml = fs.readFileSync(path.join(templates, 'core.toml'), 'utf8');
const panelToml = fs.readFileSync(path.join(templates, 'panel.toml'), 'utf8');
const envFile = fs.readFileSync(path.join(templates, 'env'), 'utf8');

describe('docker-compose 模板', () => {
  it('包含 core / panel / napcat 三个服务与专用网络', () => {
    for (const service of ['core:', 'panel:', 'napcat:']) {
      assert.match(compose, new RegExp(`^\\s{2}${service}`, 'm'), `缺少服务 ${service}`);
    }
    assert.match(compose, /name: echo-agent/);
    assert.match(compose, /network/);
  });

  it('面板端口 8080 与 NapCat WebUI 6099 暴露到宿主', () => {
    assert.match(compose, /"8080:8080"/);
    assert.match(compose, /"6099:6099"/);
  });

  it('core 挂载 /config 与 /data，并注入 ECHO_MEDIA_DIR=/data/media', () => {
    assert.match(compose, /\.\/config:\/config/);
    assert.match(compose, /\.\/data:\/data/);
    assert.match(compose, /ECHO_MEDIA_DIR=\/data\/media/);
  });

  it('镜像可用 .env 覆盖（默认 ghcr 发布镜像）', () => {
    assert.match(compose, /\$\{ECHO_CORE_IMAGE:-ghcr\.io\/echolab-auto\/echo-agent-core:latest\}/);
    assert.match(envFile, /^ECHO_CORE_IMAGE=ghcr\.io\/echolab-auto\/echo-agent-core:latest/m);
    assert.match(envFile, /^ECHO_PANEL_IMAGE=ghcr\.io\/echolab-auto\/echo-agent-panel:latest/m);
  });

  it('docker compose config 校验通过（无 docker 时跳过）', (t) => {
    const probe = spawnSync('docker', ['compose', 'version'], { stdio: 'ignore' });
    if (probe.status !== 0) {
      t.skip('环境无 docker compose，跳过');
      return;
    }
    const result = spawnSync(
      'docker',
      ['compose', '--project-directory', templates, '--file', path.join(templates, 'docker-compose.yml'), 'config', '-q'],
      { encoding: 'utf8' },
    );
    assert.equal(result.status, 0, `compose 校验失败：\n${result.stderr}`);
  });
});

describe('core.toml 容器模式模板', () => {
  it('LLM 基本字段齐全（api_key 空待填）', () => {
    assert.match(coreToml, /provider = "deepseek"/);
    assert.match(coreToml, /model = /);
    assert.match(coreToml, /base_url = /);
    assert.match(coreToml, /^api_key = ""/m);
  });

  it('技能目录指向镜像内路径', () => {
    assert.match(coreToml, /skills_dir = "\/app\/skills"/);
  });

  it('QQ 寻址全走 compose 服务名（NapCat 容器由 compose 管理）', () => {
    assert.match(coreToml, /napcat_auto_start = false/);
    assert.match(coreToml, /napcat_container = "napcat"/);
    assert.match(coreToml, /napcat_host = "core"/);
    assert.match(coreToml, /napcat_onebot_url = "http:\/\/napcat:3000"/);
    assert.match(coreToml, /napcat_webui_url = "http:\/\/napcat:6099"/);
    // 只检查生效配置行（注释里允许解释性提及 localhost）
    const effective = coreToml
      .split('\n')
      .filter((line) => !line.trim().startsWith('#'))
      .join('\n');
    assert.doesNotMatch(effective, /localhost|host\.docker\.internal/);
  });

  it('管理面监听容器全网卡；下载目录在持久卷内', () => {
    assert.match(coreToml, /management_address = "0\.0\.0\.0:3132"/);
    assert.match(coreToml, /dir = "\/data\/downloads"/);
  });

  it('默认人格启用管理面插件（否则面板连不上会话）', () => {
    assert.match(coreToml, /echo-agent\.management\.panel/);
  });
});

describe('panel.toml 容器模式模板', () => {
  it('Core 地址走 compose 服务名', () => {
    assert.match(panelToml, /connect_url = "ws:\/\/core:3132"/);
    const effective = panelToml
      .split('\n')
      .filter((line) => !line.trim().startsWith('#'))
      .join('\n');
    assert.doesNotMatch(effective, /localhost/);
  });

  it('媒体目录与 compose 注入的 ECHO_MEDIA_DIR 一致（否则图片 404）', () => {
    const injected = compose.match(/ECHO_MEDIA_DIR=(\S+)/)[1];
    const mediaDir = panelToml.match(/media_dir = "([^"]+)"/)[1];
    assert.equal(injected, mediaDir, 'ECHO_MEDIA_DIR 与 panel media_dir 必须一致');
  });

  it('监听 0.0.0.0:8080，静态目录为镜像内 /app/web', () => {
    assert.match(panelToml, /bind_address = "0\.0\.0\.0:8080"/);
    assert.match(panelToml, /static_dir = "\/app\/web"/);
  });
});

describe('跨模板一致性', () => {
  it('compose 的 napcat 容器名 == core.toml 的 napcat_container', () => {
    const containerName = compose.match(/container_name: (\S+)/g).map((line) => line.split(' ')[1]);
    assert.ok(containerName.includes('napcat'));
    assert.match(coreToml, /napcat_container = "napcat"/);
  });

  it('core 与 panel 共享同一份 config / data 挂载（媒体与配置同源）', () => {
    const mounts = compose.match(/\.\/data:\/data/g) ?? [];
    assert.equal(mounts.length, 2, 'core 与 panel 都应挂载 ./data:/data');
    const configMounts = compose.match(/\.\/config:\/config/g) ?? [];
    assert.equal(configMounts.length, 2, 'core 与 panel 都应挂载 ./config:/config');
  });
});
