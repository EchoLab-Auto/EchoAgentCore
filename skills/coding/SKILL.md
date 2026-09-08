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

## When to use

- User asks "查看 xxx 文件" / "打开 xxx" → `read_file` with the path
- User asks "项目结构" / "有哪些文件" → `list_files`
- User asks "xx 函数在哪里" / "找一下 xx" → `search_code`
- User asks "这个函数怎么实现的" → `search_code` to find the definition, then `read_file` to read it

## Safety

- All file access is restricted to the project workspace
- `read_file` returns content with line numbers for easy reference
- `search_code` limits to 50 results to avoid overwhelming output
