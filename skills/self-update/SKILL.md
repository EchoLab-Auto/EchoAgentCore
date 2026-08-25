---
name: self-update
description: 框架自更新与部署验证（Core/Panel 更新、用户告知、中断注意）
keywords: [自更新, 更新框架, 部署, update, 升级, 重启服务]
---

# 自更新（Self-Update）技能

本框架采用"原子二进制替换 + 进程重启"的自更新机制。执行前后请遵循以下流程与经验。

## 核心认知：自更新 = 对话中断

- Core 服务进程就是 Agent 自己。`update.sh` 构建成功后执行
  `systemctl --user restart echo-agent-core.service`，进程重启 = 当前对话
  中断，期间 Agent 无法回复。
- **必须在执行前告知用户**："即将更新 Core，对话可能中断片刻，完成后会恢复"。
  用户明确同意后再执行。
- 更新完成后会话上下文自动恢复（`echo-sessions-{id}.json` 持久化），
  但进行中的一轮对话会被打断。

## 更新流程（Core）

```bash
cd ~/.local/share/echo-agent-core/source
bash ~/.local/libexec/echo-agent-core/update.sh
```

- 构建产物：`target/release/echo-agent-core`
- 替换目标：`~/.local/libexec/echo-agent-core/echo-agent-core-bin`
- 状态文件：`~/.local/state/echo-agent-core/update-status`
  （state=running → building → done；服务失败时可能停在 failed）

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
6. 若 update-status 停在 state=running 且服务已重启：人工补写 state=done
   （构建脚本可能在超时中断，但二进制已替换、服务已重启，仅为状态残留）。

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
