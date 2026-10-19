---
package: echo-agent.subagent
name: model-profiles
description: 模型供应商与子代理模型选择规范 — 默认一律用默认模型（不传 profile 继承主 agent）；仅当用户明确点名某模型（opus/kimi/gemini/gpt）时才用 profile 切换；用 list_api_profiles 查询可用供应商
metadata:
  always: true
keywords: [模型, 供应商, profile, kimi, k3, deepseek, 子代理模型, 视觉, 截图, 图形, 前端, spawn_subagent]
---

# 模型供应商与子代理模型选择规范

## 查询可用供应商（第一步）

委派子代理前（或需要了解"有哪些模型可用"时），调用 **`list_api_profiles`**
工具：返回全部 API profile（名称 / 服务商 / 模型 / 端点 / API Key 就绪状态）
与顶层默认。`spawn_subagent` 的 `profile` 参数只能填该清单中的名称。

## 子代理模型选择（2026-10-17 用户设定：默认即默认模型；指定才切换）

**核心规则：一切子代理任务默认用「默认模型」（不传 `profile`，继承主 agent）。
只有用户明确点名某个模型时，才使用 `profile` 切换。**

| 情况 | 子代理模型 |
| --- | --- |
| **默认（绝大多数情况：调研 / 诊断 / 计划 / 设计 / 写码 / 测试 / 部署 / 批量 / 文档…）** | **不传 `profile`** —— 继承主 agent 的默认模型 |
| 用户明确说「用 opus / 用 uu-api / 用 claude」 | `profile: "uu-api"`（Claude Opus 5.5） |
| 用户明确说「用 kimi / k3」 | `profile: "kimi"`（Kimi K3） |
| 用户明确说「用 gemini」 | `profile: "uuapi-gemini"`（Gemini 4 Argon） |
| 用户明确说「用 gpt / spacetime」 | `profile: "spacetime"` 或 `"uuapi"`（按用户指名） |

**禁止擅自升级模型**：不得因为「任务看起来高端 / 重要」就自行切到 opus 等更贵
的模型；也不得为省钱而擅自降级用户点名的模型。用户点名 = 严格执行；未点名 =
默认模型。

> 说明：本规则取代此前「opus 做计划、deepseek 做执行」的分工约定（2026-10-17
> 同日修订）。若用户后续要求恢复按阶段分工，直接改本节即可。

## 行为与边界

- **fail-closed**：`profile` 名字不存在 / 缺 key 时子任务**立即失败**，并以
  `<subagent_event>` 回报**可用清单**——按清单纠正后重发即可；绝不静默回退
  主模型（避免"以为用了便宜模型、实际烧了贵模型"）。
- 主 agent 自身模型由人格配置（`api_profile`）决定；本规范只影响**子代理
  委派**（`spawn_subagent`），不影响主对话。
- 规则可随时调整：直接编辑本文件（`skills/model-profiles/SKILL.md`）即可，
  无需改代码。
