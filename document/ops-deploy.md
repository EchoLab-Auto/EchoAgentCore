---
group: 运维模块
x: 591
y: 250
---

# 部署与自更新

EchoAgent 以 systemd **用户服务**运行（Core + Panel 各自独立）。源码托管于受管 git 仓库，更新走 oneshot 更新单元（构建 → 原子替换 → 重启）。

## 服务与文件布局

| 组件 | 服务单元 | 二进制/静态目录 | 管理仓库 |
| --- | --- | --- | --- |
| Core | `echo-agent-core.service` | `~/.local/libexec/echo-agent-core/echo-agent-core-bin` | `~/.local/share/echo-agent-core/source` |
| Panel | `echo-agent-panel.service` | `~/.local/libexec/echo-agent-panel/{echo-agent-panel-bin,web}` | `~/.local/share/echo-agent-panel/source` |
| 更新 | `echo-agent-core-update.service` / `echo-agent-panel-update.service` | — | 同上 |

- Core 工作目录：`~/.local/share/echo-agent-core/source`；配置：`~/.config/echo-agent-core/core.toml`
- Panel 配置：`~/.config/echo-agent-panel/panel.toml`（`[server].bind_address`、`[core].connect_url` 指向 Core management WS）
- 更新状态：`~/.local/state/echo-agent-{core,panel}/update-status`；安装/更新回滚快照在 `~/.local/state/echo-agent-core/install-rollback/`

## 安装（install.sh）

两仓库各自提供 `scripts/install.sh`（Linux + systemd user，无需 root、不动系统目录；`--dry-run` 可预演全部路径）：

- 尊重 `ECHO_PREFIX` / `XDG_CONFIG_HOME` / `XDG_DATA_HOME` / `XDG_STATE_HOME`
- 已存在的配置文件保留；重复执行 = 刷新二进制、updater 与服务单元
- 安装是事务性的：release 构建完成后快照现有文件，健康检查失败自动恢复并重启旧 Core
- 服务默认挂在 `default.target`；开机免登录自启需 `sudo loginctl enable-linger "$USER"`
- 卸载：`./scripts/uninstall.sh`（保留配置）/ `--purge`（连配置目录一起删）

### 服务加固与 sudo

长驻 `echo-agent-core.service` 设 `NoNewPrivileges=false`：`run_sudo` 人机授权流（见 [工具系统](./core-tools.md)）依赖 sudo 的 setuid 提权，`NoNewPrivileges=true` 会阻断。oneshot 更新单元保持 `NoNewPrivileges=true`——它只拉代码、构建、替换用户目录二进制，从不运行 sudo，限制被攻破构建脚本的提权面。

## 更新流程（update.sh）

- `--local`（默认）：构建当前源码树（含未提交改动，记为 `+dirty`）；`--remote`：先 fetch + fast-forward 再构建（工作树有改动/游离 HEAD/非快进则拒绝）
- **原子替换**：先备份现有二进制/静态目录到 rollback 文件，替换后重启服务并健康检查（15s 内 3 次活跃确认）
- 失败自动回滚（恢复备份 + 重启）；状态写入 `~/.local/state/echo-agent-*/update-status`
- 更新器非阻塞文件锁拒绝并发运行；拉取/构建失败时旧二进制不动、不重启
- Panel 更新包含 `npm run build`（前端产物从 `web/dist` 拷贝到静态目录，需 PATH 中有 npm——nvm 路径）
- **NapCat 恢复门控**（2026-09-13）：Core 重启后仅当 `[adapters.qq].enabled = true` 且存在 `napcat` / `echo-napcat-*` 容器时，才等待反向 WS（3131 或 3140-3399，多实例）自动重连（40s 窗口），超时才 `docker restart` 兜底（遍历所有 NapCat 容器）；QQ 未启用时整段跳过——不再无谓重启容器
- **Core 插件感知校验**（`verifying_plugins` 阶段，2026-09 修正）：安装后对二进制逐个核对内置插件 manifest id（9 个：含 `echo-agent.workspace`；含互斥循环模式插件 `echo-agent.loop.{single,parallel}`；`echo-agent.menu` 与 `echo-agent.checklist` 已移除——present_menu 降级为普通编排工具、checklist 降级为普通内置工具；旧驱动 id `loop.runner`、旧编排模式 id `orchestration.{single,chatbot}` 与旧特性 id branch.reply/session.global/chatbot.sessions 已移除）。实现先 `strings > 临时文件` 再 `grep -q`——管道直连 `strings | grep -q` 在 pipefail 下会因 grep 提前退出触发 SIGPIPE（141）误报全部 missing

### 状态机与轮询契约

```text
state=running → state=restarting → state=updated（失败时停在 failed / rolled_back）
```

- `state=restarting` 在 **systemctl restart 之前**写入（"构建完成、即将重启"）；`updated` 在重启 + QQ 恢复之后才写入（恢复段仅 QQ 启用时执行：自动重连窗口 40s + 兜底重启窗口 90s，最长约 2 分钟；未启用则直接跳过）
- **self-update 轮询契约**：agent 轮询到 `restarting` 就应立即收尾结束当前 turn（不要等 `updated`——等它的 turn 会被自己的重启杀死，时间线留下僵尸 running 工具条目）；重连后的新 turn 再验证 `updated`

### Agent 自更新授权（framework_update 工具）

Agent 不能向工具传递命令/仓库/路径/分支/服务名——仅接受 `status` 与 `apply` 两个动作，`apply` 还需 `confirm=true`；服务名编译期固定为 `echo-agent-core-update.service`。授权按真实会话 ID 判定：local 平台会话（TUI 与本机工作区通道 `local:workspace:*`）需 `allow_local = true`；QQ 会话需匹配 `[adapters.qq].owner_qq` 或 `allowed_qq_users`；其他平台一律拒绝。

```toml
[agent.self_update]
enabled = true
allow_local = true
allowed_qq_users = [123456789]
```

## 自更新注意事项

- **绝不** `systemctl --user stop echo-agent-core.service`：agent 本身运行在 core 进程内，stop 会杀死当前会话，后续 start 永远执行不到（已实际发生）
- 安全替代：`systemctl --user restart echo-agent-core.service`（原子操作）；确需先停后启时用 `systemd-run --user` 脱离会话执行
- **优雅排空（drain-then-exit）**：Core 收到 SIGTERM 后先进入排空模式——拒绝新消息（Panel 主动发送会收到"Core 正在重启"错误提示，重试即可）、等待进行中的回复完成（最多 `DRAIN_TIMEOUT_SECS = 120s`，期间回复继续实时 emit 到 Panel）再退出；超时仍未完成才强制取消该 turn；`TimeoutStopSec=180s` 兜底保证 drain 窗口内不被 SIGKILL。停机顺序：`pump.abort()`（停命令泵）→ `qq_adapter.stop()`（QQ 停收）→ `agent.shutdown()`（drain）。后台任务/定时器不在 `active_inbound_turns` 计数内，drain 不等待它们；其持久化结果在重启后仍会投递（事件溯源）
- Core 重启后恢复时间线时，进程被杀导致的悬空 running 工具条目会被标注为"已中断"，不再永远显示"运行中"

## 日常体检

```bash
systemctl --user status echo-agent-{core,panel}.service
systemctl --user is-active echo-agent-{core,panel}.service
cat ~/.local/state/echo-agent-{core,panel}/update-status
journalctl --user -u echo-agent-core-update.service
```

## 回滚

更新失败时 update.sh 自动回滚；人工回滚可从保留的 rollback 文件恢复（`~/.local/libexec/echo-agent-*/` 下的 `.rollback`），或从 git 仓库构建旧提交。
