---
id: adr-0015
title: "ADR-0015 优雅排空"
group: 架构决策
x: 2076
y: 1392
---
# 0015 Graceful Drain — 自更新不打断当前回复

日期: 2026-08-26
状态: 已实现

## 问题

自更新 = 二进制替换 + Core 进程重启。旧实现 `Agent::shutdown` 收到
SIGTERM（`systemctl restart` 自动发出）后**立即 cancel 所有进行中的 turn**，
正在生成的回复被掐断——用户看到"对话中断"。

## 方案：排空优先（drain-then-exit）

重启不再立即杀死 turn，而是：

1. **进入 draining 状态**：拒绝新消息（进程即将退出，新 turn 没必要开始）；
   向 Panel 发一个 Error 事件提示"Core 正在重启，等待当前回复完成…"
2. **等待活跃 turn 清空**：轮询 `active_inbound_turns`（最多
   `DRAIN_TIMEOUT_SECS = 120s`）。期间进行中的回复继续正常 emit 到
   management WS，Panel 实时看到完整输出
3. **超时兜底**：120s 仍未完成才强制取消（长任务给出容忍上限，避免卡死重启）
4. 正常退出路径不变：cancel 剩余 → save_now 落盘 → 进程退出 → systemd
   拉起新二进制 → Panel 自动重连 → 时间线恢复，会话无缝继续

## 触发链路（无需改 update.sh）

- `update.sh` 完成构建后执行 `systemctl --user restart`
- systemd 先发 SIGTERM → Core 主循环 `shutdown_signal()` → 顺序执行：
  `pump.abort()`（停命令泵，新消息不再入队）→ `qq_adapter.stop()`
  （QQ 侧停收）→ `agent.shutdown()`（drain）
- systemd `TimeoutStopSec` 从 30s 提到 180s，保证 drain 窗口内进程不被
  SIGKILL

## 用户可见行为

- 更新时若正在回复：回复完整输出 → toast 提示重启 → Panel 重连 →
  上下文自动恢复。感知上"回复完成后短暂掉线再回来"，不会看到半截话
- 更新时无活跃 turn：秒级重启，几乎无感
- 超时（>120s 的长任务）：强制取消，该 turn 失败——长任务用户应等待
  完成后再更新（self-update skill 已提示）

## 边界

- drain 期间拒收的新消息：QQ 适配器已停止（不会收到）；Panel 主动发送
  会得到"Core 正在重启"错误提示，重试即可
- 后台任务（spawn_background_task / timers）不在 `active_inbound_turns`
  计数内，drain 不等待它们；其持久化结果在重启后仍会投递（事件溯源）
