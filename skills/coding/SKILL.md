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
- `bash` — Run a terminal command (e.g. `git status`, `ls -la`). Window is 120s by default, at most 300s via `timeout_secs`; workspace-restricted, dangerous commands blocked. **Build / test / install / download / clone / training / data-processing / whole-disk-scan commands must run in the background — do not just stretch the window** (see 「Long-running processes」below).
- `bash-start` — Start a persistent background bash session (keeps cwd/env/background jobs across commands). Returns a session id. Optional `workdir`.
- `bash-exec` — Run one command inside an existing session (`session_id` + `command`). Output streams to the Panel live; on timeout the session is **kept**, so you can poll again.
- `bash-stop` — Stop a session and kill its whole process group (bash + all descendants, including background jobs).

> 这三个工具由 `echo-agent.shell` 模块提供，只有该模块在人格白名单里才可用。
> **不在白名单时**用下面「没有 shell 模块时」的写法，功能等价。

## Long-running processes

**后台化的判据是命令类别，不是估出来的秒数**（模型估不准耗时）。下面这些
**默认后台化**，第一动作是切后台，不是调大 `timeout_secs`：

| 类别 | 典型命令 |
| --- | --- |
| 编译 / 构建 | `cargo build`、`cargo check --workspace`、`make`、`cmake --build`、`npm run build`、`vite build`、`docker build` / `compose build` |
| 测试 | `cargo test`、`pytest`、`vitest run`、`playwright test` |
| 依赖安装 | `npm/pnpm/yarn install`、`pip install`、`uv sync`、`cargo install`、`apt install` |
| 训练 / 推理 / 数据处理 | `python train.py`、`torchrun`、`accelerate launch`、大 pkl/joblib 加载、数据集转换 |
| 下载 / 克隆 / 打包 | `git clone`、大仓库 `git fetch/pull`、`wget`/`curl` 大文件、数据集解压、`tar`、`rsync` |
| 大范围扫描 | `rg`/`find` 扫整个盘或家目录、`grep -r` 全仓、`du` |
| 常驻 / 持续输出 | dev server、watch 构建、`tail -f` |

可以直接前台跑的：`git status` / `diff` / `log`、`ls`、`sed -n`、`head`/`tail` 单文件、
单文件 `grep`、`curl -m 5` 这类带短超时的探测、单包增量 `cargo check`。

判不准就后台化：代价不对称——后台化只多一轮 poll（约 2 秒），前台卡住则白等
满 120s、拿不到任何输出、还得重跑。`bash` 的 120s 窗口是硬边界，调大到 300s
也只是把超时推迟，那 300s 里拿不到任何中间结果；而 `bash-exec` 到点**不销毁
会话**，下一轮再 poll 就行。

要跑常驻进程（文档/开发服务器、watch 构建、本地服务等）时，**优先用
`bash-start` + `bash-exec`**（前提：该人格启用了 `echo-agent.shell`）：

- 会话有 session_id，会出现在 Shell 视图里（可见、输出可回读）；
  `bash-stop` 终止整个会话进程组（bash 及其全部子孙，含后台任务）；
  用 `nohup` 挂的进程脱离框架，只能手动 kill，机器重启即丢失且无人知晓
- 同一 session 内命令保持 cwd 与环境变量（先 `cd` 再启动，或 `bash-start`
  传 workdir）
- 例子：`bash-start(workdir=项目目录)` → `bash-exec(session_id, "echo-prodoc view document/")`
  → 结束时 `bash-stop(session_id)`
- **需要跨 Core 重启存活 / 开机自启 / 对外长期可达**的服务（文档站点、长期
  服务等）**不属于 shell 会话模型**：用 `systemd-run --user --unit=<名称>` 交给
  用户级 systemd 托管（`systemctl --user status/stop <名称>` 管理）——shell 会话
  是运行期资源，Core 重启即回收，刻意不承担跨重启职责

### 没有 shell 模块时：`bash` 也能安全后台化

`bash` 工具用 `wait_with_output()` 等 stdout 到 EOF，所以**裸 `&` 仍会阻塞到
子进程结束**（实测 `(sleep 8; echo A) &` 阻塞满 8 秒）。必须**重定向 + 哨兵**，
两者缺一不可——加上之后实测 **2ms 返回**：

```bash
# 一句一条：后台起任务 + 等哨兵 + 拿尾部（不会空转，条件满足立刻返回）
(cargo test --workspace > /tmp/t.log 2>&1; echo "EXIT=$?" >> /tmp/t.log) &
until grep -q '^EXIT=' /tmp/t.log 2>/dev/null; do sleep 2; done
tail -20 /tmp/t.log
```

要**这一轮就返回、不阻塞**（同轮还要发别的独立调用），去掉 `until` 两行即可：

```bash
(cargo build --release > /tmp/b.log 2>&1; echo "EXIT=$?" >> /tmp/b.log) &
```

下一轮再 `until grep -q '^EXIT=' /tmp/b.log 2>/dev/null; do sleep 2; done; tail -20 /tmp/b.log`。
**没有 `EXIT=` 哨兵时 `until` 会永远等下去**——哨兵和重定向必须成对出现。

## When to use

- User asks "查看 xxx 文件" / "打开 xxx" → `read_file` with the path
- User asks "项目结构" / "有哪些文件" → `list_files`
- User asks "xx 函数在哪里" / "找一下 xx" → `search_code`
- User asks "这个函数怎么实现的" → `search_code` to find the definition, then `read_file` to read it

## 等待：等条件，不要等时间

**不要用无条件长 `sleep` 等结果**——等就等条件：

```bash
# 等后台任务
(job > log 2>&1; echo DONE >> log) & until grep -q DONE log 2>/dev/null; do sleep 2; done; cat log
# 等服务就绪
for i in $(seq 1 60); do [ "$(systemctl --user is-active x)" = active ] && break; sleep 2; done
```

轮询比 `sleep` **更快**（条件满足立刻返回），且不会撞上工具 120s 超时只拿到
一个空错误。实测数字、四种场景的完整写法和例外情况见
`long-task-efficiency` §3（常驻技能）。

## Safety

- **路径约定**：相对路径相对工作区解析且必须落在工作区内（`../..` 逃逸、
  指向区外的符号链接会被拒绝，写工具在创建目录前就拒绝）；**绝对路径原样
  使用**——多仓库工作流（如工作区在 Core、同时要改 Panel / ui-frame）用绝对
  路径直接读写，不必退回 `bash`
- `read_file` returns content with line numbers for easy reference
- `search_code` limits to 50 results to avoid overwhelming output
