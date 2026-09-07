---
id: skills
title: "技能系统"
group: 后端模块
x: 1298
y: 1333
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

- `ReloadSkills` 命令手动重载 skills_dir；`plugins_dir` 数据插件目录 5s 轮询自动热挂载（技���/工具皆可热插拔）
- 重载**继承运行时启停状态**（`inherit_enabled_from`）：文件更新/新增/删除不丢失用户在面板的启停选择
- 启停持久化于 `[agent].disabled_skills`，重启后保持；Panel 设置视图可浏览/启停/编辑/删除技能（含正文）

## 系统提示词技能（system: true）—— 可插拔身份层

系统提示词也可作为 skill 插拔：**任意 SKILL.md 声明 `system: true`**（顶层或 `metadata:` 下）
即成为可插拔系统提示词，与业务技能（知识/操作指南）区分——它是"身份/规则"层。

- **注入**：所有**启用**的 system skills 在构建系统提示词时注入 base 区
  （`PromptBlock` kind = `system-skill`，标签"系统提示词 · 名称"，按名称排序）；
  `always`/关键词与否**不影响** system 注入——只要启用就注入
- **人格级引用**：`TeamMember.system_skills`（名字列表）非空时，这些 skill 追加为
  该 agent 的人格系统提示词层；为空回退 `system_prompt` 文本（旧配置兼容）。
  注入层级：全局 system skills → 人格 system_skills → 人格 system_prompt 字段
- **生命周期完全复用技能系统**：可新建/编辑/启停（`[agent].disabled_skills`）/删除/
  Git 安装/热重载——身份规则与业务知识同样可插拔，默认全局启用、可按 agent 白名单关闭
- **保存入口**：`SaveSkill.system` 字段（写入 SKILL.md frontmatter `system: true`）；
  `SkillInfo.system` 由 SkillsList 携带，面板显示"系统提示词"徽标；
  `SaveTeam.system_skills` 保存人格引用列表
- **面板**：技能编辑表单"系统提示词"开关 + 智能体（Team）编辑"系统提示词 skills"勾选

## 外部 Git 来源技能

技能可从外部 Git 仓库安装/更新（`skill_install.rs` + `InstallSkillFromGit`/`UpdateSkillFromGit`/`RemoveSkillSource` 三命令），Panel 设置视图「技能」分类提供完整交互（见 [Panel 交互定义](./panel-interaction.md) §9.3）。

### 安装（InstallSkillFromGit）

1. **目录名**：缺省取仓库名（URL 末段去 `/` 与 `.git` 后缀）；`name` 参数显式指定；`.` 开头或含路径分隔符的名字拒绝；目标目录已存在拒绝（提示先卸载或换名）
2. **克隆**：`git clone --depth 1 [--branch X] url skills_dir/<name>`（shallow clone，shell 调系统 `git`——支持 https/ssh/本地路径，鉴权依赖本机 git 凭证/ssh agent）
3. **目录提升**：clone 后根目录无 `SKILL.md` 时，取第一个含 `SKILL.md` 的子目录（深度 ≤ 2）内容提升到安装根；找不到则清理半成品目录并报错「仓库中未找到 SKILL.md」
4. **来源记录**：写入 `skills_dir/.sources.json`（`{name: {url, rev, branch, installed_at}}`，rev = clone 后 `rev-parse HEAD`）
5. **生效**：`reload_skills` 热重载 + `emit_skills_list` 回推（SkillInfo.source 注入来源信息，前端显示 Git 徽标）+ Error 事件通知结果

### 更新（UpdateSkillFromGit）

- `git fetch --depth 1 origin` + `git reset --hard origin/<branch>`（无分支记录时用 `origin/HEAD`；指定分支 reset 失败回退 `origin/HEAD`）
- 成功后更新来源记录的 rev 与 installed_at；**fetch/reset 失败不动来源记录**（可安全重试）
- 仅对 Git 来源技能可用（无来源记录报错）；更新后同样热重载 + 回推列表

### 移除来源（RemoveSkillSource）

仅删除 `.sources.json` 中的记录，**目录保留**（技能变为普通本地技能；删除目录走 `DeleteSkill`）。Panel 对应按钮有 confirm 明示"目录保留，可手动删除"。

### 机制边界

- **同步执行**：git 子进程调用是阻塞式的，在命令处理路径上同步等待 clone/fetch 完成（网络慢时会占住该 agent 的命令处理；结果��事件异步回推，面板侧无加载态）
- **结果通知统一走 `Error` 事件**（成功/失败皆是，消息文本区分）——Panel 以 toast 呈现，这是刻意的单向命令约定：面板不预测 core 侧耗时操作
- shallow clone（`--depth 1`）：不保留历史，更新语义是"对齐远端分支头"而非版本选择；要装特定提交需先在仓库侧打分支/标签
- `.sources.json` 是唯一的来源事实来源；手工把目录拷进 skills_dir 的技能无来源记录，不显示 Git 徽标也不可「更新」
