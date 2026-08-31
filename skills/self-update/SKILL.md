---
name: self-update
description: 框架自更新与部署验证（Core/Panel 更新、用户告知、中断注意）
keywords: [自更新, 更新框架, 部署, update, 升级, 重启服务]
---

# 自更新（Self-Update）技能

本框架采用"原子二进制替换 + 进程重启"的自更新机制。执行前后请遵循以下流程与经验。

## 核心认知：自更新 = 短暂重启（已实现 Graceful Drain，不打断回复）

- Core 服务进程就是 Agent 自己。`update.sh` 构建成功后执行
  `systemctl --user restart echo-agent-core.service`。
- **Graceful Drain（决策 0015）**：收到 SIGTERM 后 Core 先进入排空模式——
  拒绝新消息、等待进行中的回复完成（最多 120s），然后才退出。因此：
  - 正在输出的回复会**完整生成完**，Panel 看到全文后再重连
  - 无活跃 turn 时近乎秒级重启，无感
  - 超过 120s 的长任务才会被强制打断（待任务完成再更新）
- systemd `TimeoutStopSec=180s` 保证排空窗口内不被 SIGKILL。
- **仍必须在执行前告知用户**（涉及重启与短暂断连）："即将更新 Core，
  当前回复会先完成，随后面板短暂重连；确认执行？"。
- 更新完成后会话上下文自动恢复（`echo-sessions-{id}.json` 持久化）。

## 更新流程（Core）

```bash
cd ~/.local/share/echo-agent-core/source
bash ~/.local/libexec/echo-agent-core/update.sh
```

- 构建产物：`target/release/echo-agent-core`
- 替换目标：`~/.local/libexec/echo-agent-core/echo-agent-core-bin`
- 状态文件：`~/.local/state/echo-agent-core/update-status`
  （state=running → restarting → updated；服务失败时可能停在 failed）

## ⚠️ 轮询契约：看到 restarting 就立即收尾（防自杀）

**不要轮询等待 `state=updated`/`done`**：updated 在重启 + QQ 恢复
（最长约 2 分钟）之后才写入，等它的 turn 会被自己的重启杀死——
排空窗口耗尽后 SIGKILL，命令永远拿不到结果，Panel 时间线上留下
一条永远"运行中"的僵尸工具行（已实际发生多次）。

正确做法：

1. 启动 update.sh 后轮询 `state=`，间隔 5-10s；
2. **一看到 `state=restarting`（构建完成、即将重启）就立即停止轮询**，
   直接结束当前 turn，告知用户："更新已构建安装，服务正在重启
   （进行中的回复会先完成），面板将短暂重连，稍后验证即可"；
3. 重启后面板重连，再读取状态文件确认 `state=updated` 完成验证。
   （重启后的验证属于新 turn，不受重启影响。）

## 更新流程（Panel）

```bash
# 注意：update.sh 需要 PATH 中有 npm（nvm 路径）
export PATH="$HOME/.nvm/versions/node/v22.22.3/bin:$PATH"
cd ~/.local/share/echo-agent-panel/source
bash ~/.local/libexec/echo-agent-panel/update.sh \
  --source "$PWD" \
  --binary ~/.local/libexec/echo-agent-panel/echo-agent-panel-bin \
  --status ~/.local/state/echo-agent-panel/update-status \
  --static-dir ~/.local/libexec/echo-agent-panel/web
```

- Panel 重启不影响 Core 对话，但前端页面需要刷新（HTML 已设 no-store，
  普通 F5 即取最新）。
- 构建失败常见原因：`required command not found: npm` → 检查 PATH。

## 更新后验证清单

1. 服务状态：`systemctl --user is-active echo-agent-core`（active）
2. 启动时间：`systemctl --user show echo-agent-core --property=ActiveEnterTimestamp`
3. 新二进制生效：比较二进制 mtime 与重启时间是否一致
   `ls -la ~/.local/libexec/echo-agent-core/echo-agent-core-bin`
4. 功能符号：`strings <二进制> | grep -c "<新增标识>"`（如功能名、插件 id）
5. 启动日志：`journalctl --user -u echo-agent-core --no-pager -n 40`
   - 应看到 `agent supervisor ready agents=[...]`、
     `plugins mounted plugins=[...]`、`sessions restored` 等行
6. 若 update-status 停在 state=running/restarting 且服务已重启：人工补写
   state=updated（构建脚本可能在超时中断，但二进制已替换、服务已重启，
   仅为状态残留）。

## 硬性红线（来自实际事故）

- **禁止 `systemctl --user stop echo-agent-core`**：会杀死自己的进程，
  命令中断在 stop 一步，后续 start 永远执行不到（已实际发生两次）。
- 需要重启用 `systemctl --user restart`（原子操作）。
- 需要先停后启（清理数据）时，用脱会话单元：
  ```bash
  systemd-run --user --collect bash -c \
    'systemctl --user stop echo-agent-core.service; <操作>; systemctl --user start echo-agent-core.service'
  ```

## 用户告知模板

> 我将更新框架（Core）：构建完成后会自动重启服务，当前对话可能中断
> 片刻，完成后会自动恢复。确认执行？

若用户本次会话正在依赖 Core（如长任务执行中），优先等待任务完成再更新。
