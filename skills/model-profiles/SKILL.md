---
package: echo-agent.subagent
name: model-profiles
description: 模型供应商与子代理模型选择规范 — 通常用 deepseek-flash（不传 profile 继承）；UI 前端图形与视觉闭环作业委派子代理时推荐 kimi(k3)；用 list_api_profiles 查询可用供应商
metadata:
  always: true
keywords: [模型, 供应商, profile, kimi, k3, deepseek, 子代理模型, 视觉, 截图, 图形, 前端, spawn_subagent]
---

# 模型供应商与子代理模型选择规范

## 查询可用供应商（第一步）

委派子代理前（或需要了解"有哪些模型可用"时），调用 **`list_api_profiles`**
工具：返回全部 API profile（名称 / 服务商 / 模型 / 端点 / API Key 就绪状态）
与顶层默认。`spawn_subagent` 的 `profile` 参数只能填该清单中的名称。

## 子代理模型选择（默认策略，2026-10 用户设定）

| 场景 | 子代理模型 |
| --- | --- |
| **通常情况下**（调研 / 代码 / 文档 / 验证 / 批量处理…） | **deepseek-flash** —— 不传 `profile`，继承主 agent 模型即可 |
| **UI 前端图形**（HTML/CSS/SVG/Canvas/图表/组件样式的生成或修改） | `profile: "kimi"`（Kimi K3） |
| **带视觉闭环的作业**（截图 → 观察 → 修改 → 再截图的迭代验证） | `profile: "kimi"`（Kimi K3） |

用法：`spawn_subagent(task: "…", profile: "kimi")` —— **单次生效**，父 agent
自身模型不变；多个子任务可以并行用不同模型。

## 行为与边界

- **fail-closed**：`profile` 名字不存在 / 缺 key 时子任务**立即失败**，并以
  `<subagent_event>` 回报**可用清单**——按清单纠正后重发即可；绝不静默回退
  主模型（避免"以为用了便宜模型、实际烧了贵模型"）。
- 主 agent 自身模型由人格配置（`api_profile`）决定；本规范只影响**子代理
  委派**（`spawn_subagent`），不影响主对话。
- 规则可随时调整：直接编辑本文件（`skills/model-profiles/SKILL.md`）即可，
  无需改代码。
