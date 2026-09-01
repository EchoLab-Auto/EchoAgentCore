---
id: skills
title: "技能系统"
group: 后端模块
x: 970
y: 1824
---

# 技能系统

技能是 SKILL.md 驱动的能力包，为模型注入特定领域的指令与流程知识。`Skill`/`SkillMetadata` 词汇与 `SkillProvider` 接缝在 echo-defs；具体实现 `SkillRegistry`（文件发现 + 热重载）在 echo-agent。

## SKILL.md 与发现

- `[agent].skills_dir` 目录下**递归发现**所有 `SKILL.md`（仓库内为 `skills/`）
- 文件 = YAML-ish frontmatter + Markdown 正文：`name` / `description` / `keywords` / `always` / `category` / `package`
- **渐进披露**：名称与描述进入系统提示词的技能清单，正文仅在常驻或触发时注入——控制提示词体积

## 常驻与触发

- **常驻技能**（`always: true` 且启用）：每轮对话都注入正文（`always_enabled`）
- **触发技能**：消息命中 `keywords` 时注入（`find_matching`；多命中时按名称排序取第一个，保证确定性）
- 注入位置：系统提示词的"常驻/触发技能"块（见 [Agent 循环](./core-agent-loop.md) 的提示词按块构建）

## 热重载与启停

- `ReloadSkills` 命令手动重载 skills_dir；`plugins_dir` 数据插件目录 5s 轮询自动热挂载（技能/工具皆可热插拔）
- 重载**继承运行时启停状态**（`inherit_enabled_from`）：文件更新/新增/删除不丢失用户在面板的启停选择
- 启停持久化于 `[agent].disabled_skills`，重启后保持；Panel 资源页可浏览/启停/编辑/删除技能（含正文）
