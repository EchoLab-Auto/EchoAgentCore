//! CLI 参数解析与命令分发（无外部依赖）。
//!
//! 用法：`echo-agent <命令> [选项]`；`--dir` 是唯一全局选项。

import { doctor, down, init, logs, restart, status, up, update } from './commands.js';
import { packageVersion, resolveDeployDir } from './paths.js';

const HELP = `echo-agent — EchoAgentCore + Panel + NapCat 一键部署（Docker）

用法：
  echo-agent <命令> [选项]

命令：
  init        生成部署文件（compose + 配置模板 + .env）
  up          拉取镜像并启动 core + panel + napcat（未初始化会自动初始化）
  down        停止并移除容器（保留配置与数据）
  restart     重启所有容器（修改配置后用它生效）
  update      拉取最新镜像并重建容器
  logs [服务] 查看日志（-f 跟随；服务名：core | panel | napcat）
  status      查看容器状态
  doctor      环境自检（docker / 端口 / 容器名冲突）
  version     打印版本
  help        显示本帮助

选项：
  --dir <路径>  部署目录（默认 $ECHO_AGENT_HOME 或 ~/.echo-agent）
  --force       init 时覆盖已存在文件

示例：
  npx @echolab-auto/echo-agent up
  npx @echolab-auto/echo-agent logs core -f
  npx @echolab-auto/echo-agent doctor --dir ./my-stack

部署完成后：
  面板      http://localhost:8080
  QQ 登录   http://localhost:6099（首次扫码）
`;

/** 解析 argv（支持 `--dir x`、`--dir=x`、`-f`、`--tail N`、位置参数）。 */
function parseArgs(argv) {
  const options = { dir: undefined, force: false, follow: false, tail: 200, positionals: [] };
  for (let i = 0; i < argv.length; i += 1) {
    const arg = argv[i];
    if (arg === '--dir') {
      options.dir = argv[++i];
      if (!options.dir) throw new Error('--dir 需要一个路径参数');
    } else if (arg.startsWith('--dir=')) {
      options.dir = arg.slice('--dir='.length);
    } else if (arg === '--force') {
      options.force = true;
    } else if (arg === '-f' || arg === '--follow') {
      options.follow = true;
    } else if (arg === '--tail') {
      const raw = argv[++i];
      const parsed = Number.parseInt(raw ?? '', 10);
      if (!Number.isFinite(parsed) || parsed <= 0) throw new Error('--tail 需要一个正整数');
      options.tail = parsed;
    } else if (arg === '-h' || arg === '--help' || arg === '--version') {
      // 帮助/版本与命令同级处理（不当作未知选项）
      options.positionals.push(arg);
    } else if (arg.startsWith('-')) {
      throw new Error(`未知选项：${arg}（-h 查看帮助）`);
    } else {
      options.positionals.push(arg);
    }
  }
  return options;
}

/** CLI 主入口；返回进程退出码（由 bin 退出）。 */
export async function main(argv) {
  const options = parseArgs(argv);
  const [command, ...rest] = options.positionals;
  const dir = resolveDeployDir(options.dir);

  switch (command) {
    case undefined:
    case 'help':
    case '-h':
    case '--help':
      process.stdout.write(HELP);
      return 0;
    case 'version':
    case '--version':
      console.log(packageVersion());
      return 0;
    case 'init':
      return init({ dir, force: options.force });
    case 'up':
      return up({ dir });
    case 'down':
      return down({ dir });
    case 'restart':
      return restart({ dir });
    case 'update':
      return update({ dir });
    case 'logs':
      return logs({ dir, service: rest[0], follow: options.follow, tail: options.tail });
    case 'status':
      return status({ dir });
    case 'doctor':
      return doctor({ dir });
    default:
      throw new Error(`未知命令：${command}（-h 查看帮助）`);
  }
}
