---
id: tool-catalog
title: "内置工具目录"
group: 插件
x: 960
y: 1780
---

<!-- 自动生成，请勿手改：`echo-agent::tool::builtin::register_all`） -->

# 内置工具目录

> 本文件由 `source/core/tests/doc_catalog.rs` 生成并校验。
> 刷新：`UPDATE_DOCS=1 cargo test -p echo-agent-core --test doc_catalog`。
> 机制说明见 [工具系统](./core-tools.md)；插件贡献的工具见 [插件化设计](./core-plugins.md)。

共 **17** 个内置工具。

| 工具 | 分类 | 说明 |
| --- | --- | --- |
| `bash` | builtin | Run a shell (bash) command in the project workspace. Returns stdout and stderr. Use for git operations, cargo builds, fi…｜包：`echo-agent.tools.builtin` |
| `calculator` | builtin | 计算一个算术表达式，支持 + - * / ( ) 和数字。参数: expression(例如 "(2+3)*4")。｜包：`echo-agent.tools.builtin` |
| `checklist` | builtin | 管理任务检查清单。操作: create 创建清单, add 添加项(需要 item), list 查看, check 勾选完成(需要 index), uncheck 取消勾选(需要 index), remove 删除项(需要 index),…｜包：`echo-agent.tools.builtin` |
| `edit_file` | builtin | Replace lines in an existing file. Specify start_line and end_line (inclusive) to replace with new_content. Use this to …｜包：`echo-agent.tools.builtin` |
| `instance_restart` | builtin | Restart a platform instance plugin (stop then start). Useful when it is not responding.｜包：`echo-agent.tools.builtin` |
| `instance_start` | builtin | Start a platform instance plugin. Use 'all' to start all configured instances.｜包：`echo-agent.tools.builtin` |
| `instance_status` | builtin | Check the status of all platform instance plugins. Returns which are running, connected, or stopped.｜包：`echo-agent.tools.builtin` |
| `instance_stop` | builtin | Stop a running platform instance plugin. Use 'all' to stop all running instances.｜包：`echo-agent.tools.builtin` |
| `list_api_profiles` | builtin | 列出可用的模型供应商（API profile）：名称 / 服务商 / 模型 / 端点 / API Key 就绪状态。用于为 spawn_subagent 的 `profile` 参数挑选目标模型（模型选择规范见 model-profiles…｜包：`echo-agent.tools.builtin` |
| `list_files` | builtin | List files in a directory. Shows file names, sizes, and types. Use this to explore the project structure. Path may carry…｜包：`echo-agent.tools.builtin` |
| `read_file` | builtin | Read the contents of a file in the project. Returns the file content with line numbers. Use this to examine source code,…｜包：`echo-agent.tools.builtin` |
| `search_code` | builtin | Search for a pattern in source files. Returns matching file paths and line numbers. Use this to find function definition…｜包：`echo-agent.tools.builtin` |
| `shell_exec` | builtin | Run one command inside a persistent shell session (see shell_start). Output is streamed to the Panel live. Timeout: 120s…｜包：`echo-agent.tools.builtin` |
| `shell_start` | builtin | Start a persistent background bash session. Commands run in the same shell keep state (cwd, exports). Returns session_id…｜包：`echo-agent.tools.builtin` |
| `shell_stop` | builtin | Stop and destroy a persistent shell session.｜包：`echo-agent.tools.builtin` |
| `web_search` | builtin | 搜索网页获取最新信息（新闻、天气、百科、实时数据）。参数: query(搜索关键词), max_results(可选, 默认5, 最大10)。｜包：`echo-agent.tools.builtin` |
| `write_file` | builtin | Create a new file or overwrite an existing file with new content. Use this to create or update source files, config file…｜包：`echo-agent.tools.builtin` |
