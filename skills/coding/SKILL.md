---
name: coding
description: 读写与检索项目代码 — **写之前必须先把前提建全**（定义/调用方/依赖/测试/配置五问各查一次），并过一次四段式方案陈述（目标与验收/事实与假设/改动清单/风险与回退）；检索一律用 search_code / read_file 而非 bash 的 grep/sed/cat（框架看不见 bash 路径，重复读压不下去）；同一文件同段最多读 2 次且第 2 次必须放大范围；编译/测试/安装/下载/克隆/训练等长任务一律后台化
keywords: [代码, 文件, 搜索, 读取, 查看, 源码, code, file, read, search, grep, 函数, function, 前提, 前置, 调用方, 依赖, 写之前, 改代码, edit, write, 重读, 重复读, 检索]
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

## 前提先行：写之前先把前提建全

**硬规则：任何 `write_file` / `edit_file` / bash 写命令之前，先把「前提清单」查完。**
前提没建全就动手，会在改到一半发现关系搞错、再回头重改——实测同一个文件被改了
57 次（`lift3kin/service.py`）、54 次（`wm_mimic_runner.py`）、49 次（`core.toml`）。
57 次不代表这个文件要改 57 处，代表前面几十次是在拿代码库当草稿纸。

### 前提清单（五问，每问至少查一次）

改一个符号 / 接口 / 配置项之前逐条回答；答不上来就是还没建全：

| 问 | 用什么查 | 查不到时 |
| --- | --- | --- |
| 它定义在哪、签名长什么样 | `search_code` 定位 → `read_file` 一次读足（签名 + 上下文） | 先 `list_files` 确认模块存在，别猜路径 |
| 谁调用它、有几处 | `search_code`（符号名，注意边界） | 换关键词（含测试里的名字）再搜一次 |
| 它依赖什么、被谁依赖 | 读该文件的 `use` / `import` / 类型定义 | 读同层相邻文件 |
| 现有测试怎么写的 | `search_code` 测试函数名 / `read_file` 测试文件 | 记下「无测试」，方案里说明 |
| 配置 / 数据从哪来 | `read_file` 配置 + 调用方 | 问用户，不要假设 |

### 检索必须用专用工具，不要用 bash

实测：bash 读文件 **5295 次** vs `read_file` **117 次**（45 : 1）；
`grep -r` + `find` **1824 次** vs `search_code` + `list_files` **43 次**（42 : 1）。
这不只是慢，有三个硬后果：

- **框架看不见 bash 里的路径**：`cat x.py`、`sed -n '1,80p' x.py` 在 harness 眼里
  只是一串字符，无法建「本会话已读过 x.py」的索引 → 重复读压不下去（实测 48%
  的读取是重复读，其中 585 次是隔 60 秒以上重读同一文件）；
- `read_file` 带行号，改的时候直接对行，省掉再 `grep -n` 一遍；
- `search_code` 一次给出跨文件全部命中行号，不会「搜一个看一个」。

**只有需要管道 / 复杂 shell 时才用 bash 检索**（`grep … | awk | sort` 这类）。

### 读的纪律

- 同一文件同一片段内最多读 2 次；**第 2 次必须放大范围**（读整段，不是再切一小片）。
- 要改某个函数：第 1 次 `search_code` 定位，第 2 次 `read_file` 一次覆盖签名 +
  上下文；禁止「看一眼改一行」。
- 文件 >800 行或 >20K 字符：不整读，按命中行号取精确段（`read_file` 已有行号，
  或 `sed -n 'A,Bp'`）。
- 改完后的复核读数不算重复读，但只做 1 次。

### 写之前还要过一次方案陈述

五问查完不等于可以动手。动手前把计划写成四段（完整版见 `engineering-rigor` 技能）：

```text
目标与验收：做成什么样 / 怎么算做完（可执行命令或可观察现象）/ 不做什么
事实与假设：已确认（附 文件:行号）/ 仍是假设（怎么验证）
改动清单：每个文件一行，说清「为什么非要改它」
风险与回退：最可能错在哪 / 出问题怎么退
```

四段缺一段就去补信息，别开第一笔。理由：实测 12.5% 的推理记录出现
「等等 / 我搞错了 / 实际上」，7.2% 出现「换个思路 / 改成」——都是方案没定就开写，
写到一半撞墙回头改。同一个文件改 57 次就是这么来的。

### 前提缺口太大时：先只读探查，别边猜边改

信息缺口大于已知信息时（跨 3 个以上模块的契约、不熟悉的仓库、实现路径未定），
**先花一轮发一个只读探查**：子代理，或一轮里打包多条 `search_code` / `read_file`。
拿到事实清单再动第一笔。

改一步看一眼的代价是每一眼都要**一轮完整模型往返**（实测中位 5.7s），
而一次打包探查只花一轮。

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
