---
name: coding
description: Read, search, and explore code in the project
keywords: [代码, 文件, 搜索, 读取, 查看, 源码, code, file, read, search, grep, 函数, function]
---

# Coding Tools

You can interact with the project's source code using these tools:

- `read_file` — Read a file's contents with line numbers
- `list_files` — List files in a directory with sizes and types
- `search_code` — Search for a pattern across source files
- `write_file` — Create or overwrite a file with new content
- `edit_file` — Replace specific lines in a file (start_line to end_line)
- `bash` — Run a terminal command (e.g. `cargo build`, `git status`). Timeout 30s, workspace-restricted, dangerous commands blocked.
- `shell_start` — Start a persistent background shell session (keeps cwd/env across commands). Returns a session id.
- `shell_exec` — Run a command inside an existing shell session (long-running servers, watchers, builds).
- `shell_stop` — Stop and destroy a shell session.

## Long-running processes

要跑常驻进程（文档/开发服务器、watch 构建、本地服务等）时，**必须用
`shell_start` + `shell_exec`，不要用 `nohup`/`&`/disown 挂野进程**：

- shell 会话有 session_id，会出现在 Shell 视图里（可见、输出可回读）；
  `shell_stop` 终止整个会话进程组（bash 及其全部子孙，含后台任务）；
  nohup 挂的进程脱离框架，只能手动 kill，机器重启即丢失且无人知晓
- 同一 session 内命令保持 cwd 与环境变量（先 `cd` 再启动，或 `shell_start`
  传 workdir）
- 例子：`shell_start(workdir=项目目录)` → `shell_exec(session_id, "echo-prodoc view document/")`
  → 结束时 `shell_stop(session_id)`
- **需要跨 Core 重启存活 / 开机自启 / 对外长期可达**的服务（文档站点、长期
  服务等）**不属于 shell 会话模型**：用 `systemd-run --user --unit=<名称>` 交给
  用户级 systemd 托管（`systemctl --user status/stop <名称>` 管理）——shell 会话
  是运行期资源，Core 重启即回收，刻意不承担跨重启职责

## When to use

- User asks "查看 xxx 文件" / "打开 xxx" → `read_file` with the path
- User asks "项目结构" / "有哪些文件" → `list_files`
- User asks "xx 函数在哪里" / "找一下 xx" → `search_code`
- User asks "这个函数怎么实现的" → `search_code` to find the definition, then `read_file` to read it

## Safety

- **路径约定**：相对路径相对工作区解析且必须落在工作区内（`../..` 逃逸、
  指向区外的符号链接会被拒绝，写工具在创建目录前就拒绝）；**绝对路径原样
  使用**——多仓库工作流（如工作区在 Core、同时要改 Panel / ui-frame）用绝对
  路径直接读写，不必退回 `bash`
- `read_file` returns content with line numbers for easy reference
- `search_code` limits to 50 results to avoid overwhelming output
