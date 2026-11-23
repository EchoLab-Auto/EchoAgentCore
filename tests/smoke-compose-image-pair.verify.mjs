// tests/smoke-compose-image-pair.verify.mjs
//
// `smoke-compose-image-pair.sh` 的纯断言部分：只读 JSON，不碰 Docker。
//
// 为什么要单独一层：把「事实采集」（脚本里的 docker/compose 调用）与
// 「事实判定」（这里的断言）分开。断言失败时能准确指出是哪一条契约破了，
// 而不是从 grep 的退出码里猜。
//
// 输入（由 .sh 生成）：
//   --compose <file>        `docker compose config --format json` 的输出
//   --core-image <file>     `docker image inspect <core 候选镜像>` 的输出
//   --panel-image <file>    `docker image inspect <panel 候选镜像>` 的输出
//   --containers <file>     `{core: <inspect>, panel: <inspect>}`（可选，
//                           启动后用同一份断言再核对运行态）
//   --expect-core-ref / --expect-panel-ref
//   --expect-core-digest / --expect-panel-digest
//   --expect-core-id / --expect-panel-id
//   --project <compose 项目名> --net-name <网络名> --panel-port <宿主端口>
//   --modules-summary <oci-summary image 的输出>
//   --core-rev <sha40> --panel-rev <sha40>
//   --core-context-sha256 / --panel-context-sha256
//
// 退出码：0 = 全部断言通过；1 = 至少一条不过（逐条打印 not ok）。

import assert from 'node:assert/strict';
import fs from 'node:fs';

function parseArgs(argv) {
  const out = {};
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    if (!key.startsWith('--')) throw new Error(`unexpected argument: ${key}`);
    const value = argv[i + 1];
    if (value === undefined) throw new Error(`missing value for ${key}`);
    out[key.slice(2).replaceAll('-', '_')] = value;
    i += 1;
  }
  return out;
}

const args = parseArgs(process.argv.slice(2));

function required(name) {
  const value = args[name];
  if (!value) throw new Error(`missing required argument --${name.replaceAll('_', '-')}`);
  return value;
}

function loadJson(name) {
  return JSON.parse(fs.readFileSync(required(name), 'utf8'));
}

const compose = loadJson('compose');
const coreImage = loadJson('core_image');
const panelImage = loadJson('panel_image');
const containers = args.containers ? loadJson('containers') : null;

const expectCoreRef = required('expect_core_ref');
const expectPanelRef = required('expect_panel_ref');
const expectCoreDigest = required('expect_core_digest');
const expectPanelDigest = required('expect_panel_digest');
const expectCoreId = required('expect_core_id');
const expectPanelId = required('expect_panel_id');
const project = required('project');
const netName = required('net_name');
const panelPort = String(Number(required('panel_port')));
const modulesSummary = required('modules_summary');
const coreRev = required('core_rev');
const panelRev = required('panel_rev');
const coreContext = required('core_context_sha256');
const panelContext = required('panel_context_sha256');
const projectDir = required('project_dir');

let failed = 0;
let passed = 0;

function check(name, fn) {
  try {
    fn();
    passed += 1;
    console.log(`ok   ${name}`);
  } catch (error) {
    failed += 1;
    console.log(`not ok ${name}: ${error?.message ?? error}`);
  }
}

function equal(actual, expected, label) {
  assert.deepEqual(actual, expected, `${label}: ${JSON.stringify(actual)} != ${JSON.stringify(expected)}`);
}

function sorted(values) {
  return [...values].sort();
}

const digestPattern = /^sha256:[0-9a-f]{64}$/;
const serviceNames = sorted(Object.keys(compose.services ?? {}));

// ── 1. compose 解析结果：这一对镜像就是被 digest 引用的那对 ──────────────
check('compose 只启用 core/panel 两个服务（napcat 被 profile 排除）', () => {
  equal(serviceNames, ['core', 'panel'], 'services');
});

check('core/panel 的 image 逐一等于两个 @sha256 引用', () => {
  assert.equal(compose.services.core.image, expectCoreRef);
  assert.equal(compose.services.panel.image, expectPanelRef);
});

check('两个 image 引用都是 digest（不是 tag，尤其不是 :latest）', () => {
  for (const [label, ref] of [['core', expectCoreRef], ['panel', expectPanelRef]]) {
    assert.match(ref, /@sha256:[0-9a-f]{64}$/, `${label} 不是 digest 引用: ${ref}`);
    assert.doesNotMatch(ref, /:latest$/, `${label} 引用了 :latest: ${ref}`);
  }
});

check('整份 compose 配置里没有公共 :latest / ghcr.io 发布镜像', () => {
  const text = JSON.stringify(compose);
  assert.doesNotMatch(text, /:latest/, 'compose 配置仍含 :latest');
  assert.doesNotMatch(text, /ghcr\.io\/echolab-auto/, 'compose 配置仍含 ghcr.io 发布镜像');
});

// ── 2. 隔离边界：项目名 / 容器名 / 网络 / 端口 / 挂载 ─────────────────────
check('compose 项目名是本次专用名', () => {
  assert.equal(compose.name, project);
  assert.notEqual(project, 'echo-agent');
});

check('容器名带本次专用前缀，且不等于生产容器名', () => {
  const names = [compose.services.core.container_name, compose.services.panel.container_name];
  for (const name of names) {
    assert.ok(name, 'container_name 缺失');
    assert.ok(name.startsWith(`${project}-`), `${name} 不以 ${project}- 开头`);
    assert.ok(!['echo-agent-core', 'echo-agent-panel', 'napcat'].includes(name), `${name} 是生产容器名`);
  }
  assert.notEqual(names[0], names[1]);
});

check('core 不发布宿主端口，panel 只绑 127.0.0.1:<本次端口>:8080', () => {
  const corePorts = compose.services.core.ports ?? [];
  assert.deepEqual(corePorts, [], `core 不该发布端口: ${JSON.stringify(corePorts)}`);
  const panelPorts = compose.services.panel.ports ?? [];
  assert.equal(panelPorts.length, 1, `panel 端口数 ${panelPorts.length}`);
  const binding = panelPorts[0];
  assert.equal(binding.host_ip, '127.0.0.1', 'panel 必须只绑回环');
  assert.equal(String(binding.published), panelPort, 'panel 宿主端口不是本次随机端口');
  assert.equal(binding.target, 8080);
  assert.notEqual(panelPort, '8080', '不允许占用生产面板端口 8080');
  assert.notEqual(panelPort, '6099', '不允许占用 NapCat WebUI 端口 6099');
});

check('挂载只有临时目录的 config/data：没有 docker.sock、没有生产配置', () => {
  for (const name of ['core', 'panel']) {
    const volumes = compose.services[name].volumes ?? [];
    const targets = sorted(volumes.map((v) => v.target));
    equal(targets, ['/config', '/data'], `${name} 挂载点`);
    for (const volume of volumes) {
      assert.equal(volume.type, 'bind', `${name} ${volume.target} 不是 bind`);
      assert.ok(
        volume.source.startsWith(`${projectDir}/`),
        `${name} ${volume.target} 的来源不在临时项目目录: ${volume.source}`,
      );
    }
  }
  assert.doesNotMatch(JSON.stringify(compose), /docker\.sock/, 'compose 里仍有 docker.sock');
});

check('网络是本次专用名，不是生产网络 echo-agent', () => {
  assert.ok(compose.networks?.['echo-agent'], '缺少 echo-agent 网络定义');
  assert.equal(compose.networks['echo-agent'].name, netName);
  assert.notEqual(compose.networks['echo-agent'].name, 'echo-agent');
});

check('两个服务都不是 restart: unless-stopped（测试不留自启容器）', () => {
  assert.equal(compose.services.core.restart, 'no');
  assert.equal(compose.services.panel.restart, 'no');
});

check('没有 privileged / host 网络 / 外部服务凭据', () => {
  const text = JSON.stringify(compose);
  assert.doesNotMatch(text, /"privileged"\s*:\s*true/, 'privileged 容器');
  assert.doesNotMatch(text, /"network_mode"\s*:\s*"host"/, 'host 网络');
  for (const key of ['DEEPSEEK_API_KEY', 'OPENAI_API_KEY', 'ANTHROPIC_API_KEY', 'NAPCAT_']) {
    assert.ok(!text.includes(key), `compose 里出现 ${key}`);
  }
});

// ── 3. 镜像本体：x86_64、digest 可追溯、Core 是 official 模块来源 ───────
check('两张镜像都是 linux/amd64（x86_64）', () => {
  for (const [label, meta] of [['core', coreImage], ['panel', panelImage]]) {
    assert.equal(meta[0].Os, 'linux', `${label} Os`);
    assert.equal(meta[0].Architecture, 'amd64', `${label} Architecture`);
  }
});

check('镜像 Id 是内容寻址的 sha256，且 RepoDigests 含本次 digest', () => {
  const pairs = [
    ['core', coreImage, expectCoreId, expectCoreDigest, expectCoreRef],
    ['panel', panelImage, expectPanelId, expectPanelDigest, expectPanelRef],
  ];
  for (const [label, meta, id, digest, ref] of pairs) {
    assert.match(id, digestPattern, `${label} image id`);
    assert.equal(meta[0].Id, id, `${label} image id 与记录不一致`);
    assert.match(digest, digestPattern, `${label} digest`);
    const repo = ref.split('@')[0];
    assert.ok(
      (meta[0].RepoDigests ?? []).includes(`${repo}@${digest}`),
      `${label} RepoDigests 不含 ${repo}@${digest}: ${JSON.stringify(meta[0].RepoDigests)}`,
    );
  }
});

check('Core 镜像标签证明来自正式（official）外置模块锁定表', () => {
  const labels = coreImage[0].Config?.Labels ?? {};
  assert.equal(labels['io.echo.external-modules.source'], 'official');
  assert.equal(labels['io.echo.external-modules.lock'], 'packaging/external-modules.lock');
  assert.equal(labels['io.echo.external-modules.revisions'], modulesSummary);
});

check('候选镜像标签记录了源码提交 / 构建上下文（镜像↔源码可追溯）', () => {
  const coreLabels = coreImage[0].Config?.Labels ?? {};
  const panelLabels = panelImage[0].Config?.Labels ?? {};
  assert.equal(coreLabels['io.echo.smoke.core.rev'], coreRev);
  assert.equal(panelLabels['io.echo.smoke.panel.rev'], panelRev);
  assert.equal(coreLabels['io.echo.smoke.core.context'], coreContext);
  assert.equal(panelLabels['io.echo.smoke.panel.context'], panelContext);
  for (const labels of [coreLabels, panelLabels]) {
    assert.match(labels['io.echo.smoke.dirty'] ?? '', /^[01]$/, '缺少 dirty 标记');
  }
});

// ── 4. 运行态（给了 --containers 才做）──────────────────────────────────
if (containers) {
  const core = containers.core?.[0];
  const panel = containers.panel?.[0];
  check('两个容器都在运行，且用的就是那两个 digest 引用', () => {
    assert.ok(core && panel, '缺少容器 inspect 数据');
    assert.equal(core.State?.Running, true, 'core 未运行');
    assert.equal(panel.State?.Running, true, 'panel 未运行');
    assert.equal(core.Config.Image, expectCoreRef);
    assert.equal(panel.Config.Image, expectPanelRef);
  });

  check('容器实际镜像 Id 等于记录的候选镜像 Id', () => {
    assert.equal(core.Image, expectCoreId, 'core 运行镜像 id');
    assert.equal(panel.Image, expectPanelId, 'panel 运行镜像 id');
  });

  check('运行态挂载同样没有 docker.sock，且只有临时 config/data', () => {
    for (const [label, meta] of [['core', core], ['panel', panel]]) {
      const mounts = meta.Mounts ?? [];
      equal(sorted(mounts.map((m) => m.Destination)), ['/config', '/data'], `${label} mounts`);
      for (const mount of mounts) {
        assert.equal(mount.Type, 'bind', `${label} ${mount.Destination} 不是 bind`);
        assert.ok(mount.Source.startsWith(`${projectDir}/`), `${label} 挂载源越界: ${mount.Source}`);
      }
    }
  });

  check('运行态端口：core 无宿主端口，panel 仅回环随机端口', () => {
    const corePorts = core.HostConfig?.PortBindings ?? {};
    assert.deepEqual(Object.keys(corePorts), [], `core 发布了端口: ${JSON.stringify(corePorts)}`);
    const panelPorts = panel.HostConfig?.PortBindings ?? {};
    assert.deepEqual(Object.keys(panelPorts), ['8080/tcp'], 'panel 端口键');
    const bindings = panelPorts['8080/tcp'];
    assert.equal(bindings.length, 1);
    assert.equal(bindings[0].HostIp, '127.0.0.1');
    assert.equal(String(bindings[0].HostPort), panelPort);
  });

  check('运行态没有特权和自启策略', () => {
    for (const [label, meta] of [['core', core], ['panel', panel]]) {
      assert.equal(meta.HostConfig?.Privileged, false, `${label} privileged`);
      assert.equal(meta.HostConfig?.RestartPolicy?.Name, 'no', `${label} restart policy`);
    }
  });

  check('容器环境没有 LLM 凭据', () => {
    for (const [label, meta] of [['core', core], ['panel', panel]]) {
      for (const entry of meta.Config?.Env ?? []) {
        assert.doesNotMatch(
          entry,
          /^(DEEPSEEK_API_KEY|OPENAI_API_KEY|ANTHROPIC_API_KEY|NAPCAT_)/,
          `${label} 环境里有 ${entry.split('=')[0]}`,
        );
      }
    }
  });
}

console.log(`\nverify: ${passed} passed, ${failed} failed`);
process.exit(failed === 0 ? 0 : 1);
