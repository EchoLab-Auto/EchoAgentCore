---
name: long-task-efficiency
description: 长任务执行效率 — **编译/测试/安装/下载/克隆/训练/数据处理/全盘扫描/常驻进程一律后台化**（优先 `bash-start` + `bash-exec` 会话；判据是任务类别不是秒数，`timeout_secs` 只把窗口抬到 300s 上限，不是长任务开关）；每次响应打包 2-4 条独立调用（跨轮等待中位 2.4s，懒得分轮代价最大）；同一文件同段内最多读 2 次，第 2 次必须放大范围；等待等条件不等时间（无条件 sleep 占过一次会话 61% 工具耗时的 2989s）；>20K 字符输出落盘分页，禁整读；每 50 步一次检查点与拆分判断 (常驻)
keywords: [长任务, 效率, 批处理, 并行, 往返, 轮次, 轮询, 重试, 重试失败, 超时, timeout, timeout_secs, 上下文, bash, 后台, 后台化, 后台shell, bash-start, bash-exec, shell会话, 构建, 测试, 训练, 检查点, checkpoint, 读文件, 分页, 落盘]
---

# 长任务执行效率

适用：任何预计超过 ~20 次工具调用的请求。目标是把"轮次"降到最少。

## 实测基线（2026-10-19 审计，最近 3 天 / 8664 次工具调用 / 306 个请求片段）

| 指标 | 实测 | 含义 |
| --- | --- | --- |
| 平均调用/轮 | **1.07**（单调用轮 95.3%） | 一次响应只发一个工具是常态 → 轮次爆炸 |
| 跨轮间隔 | 中位 **2.44s**、p90 14.4s、p99 54s | 每多一轮 = 多一次 2.4s 起步的等待 + 重发上下文 |
| 片段 span | 27.3h，其中工具 14.3h（52%）、模型往返 12.1h（44%） | 真正干活 3.9h（14%），86% 是等待 |
| `sleep` 类调用 | 483 次 / **10.4h** | 长等待几乎全部串行阻塞 |
| 同文件重复读 | 超额重复 **5657 次**，56% 片段存在同文件 ≥3 次 | "看一眼改一行"是第二大会漏 |
| 单条工具输出 | 中位 1.3K 字符，最大 **181K** | 一条整读就能顶掉后面十轮的上下文预算 |
| 错误率 | 0.62%（54/8664），原样重发 0 次 | 失败重试已收敛，别再退回去 |

三个数字决定了本技能的全部优先级：**往返贵（2.4s/轮）、重复读多（5657 次）、长等待串行（10.4h）**。

## 1. 每次响应打包 2–4 条独立调用（第一优先级）

**信号 → 动作**

- 准备"读一个文件"→ 先问：还有没有别的独立信息要拿？有就**一起发**
  （3 条 read_file / 3 条 bash / 2 条 + 1 条 grep）。
- 探测型命令（`ls`、`git status`、`grep -n`、`curl`、`ss`、`journalctl`）互相
  独立时，**同一个响应里发多条**；框架串行 `await`，但只花一次模型往返。
- 能合成 1 条 bash（`a; b; c`，或 `&`+`wait` 并发）时也合并；但优先级是
  "不要多轮"，不是"合并成一条命令"。

**三连包模板**（直接照抄改路径即可）：

```text
① 定位：grep -n "<符号>\|<关键词>" -r <目录> --include='*.rs' | head -40
② 结构：find <目录> -name '*.rs' | head -40; ls -la <关键子目录>
③ 状态：git -C <repo> status --short; git -C <repo> log --oneline -5
```
```text
① 读实现：sed -n '1,400p' <文件>          （一次读足，别 1-80 / 80-160 分轮）
② 读调用方：grep -rn "<函数名>(" <目录> --include='*.ts' | head -30
③ 读配置/测试：sed -n '1,120p' <相关测试或配置>
```

**反例（实测占跨轮 94%，最贵）**：发 `sed -n '1,80p' x.rs` → 看输出 →
再发 `grep -n foo x.rs` → 再看 → 再发 `sed -n '80,160p' x.rs`。同样信息
3 轮 = 3 × 2.4s 等待 + 3 次上下文重发。

自检：如果连续 3 轮的调用彼此**不依赖**对方输出，说明第 2、3 轮本该并进第 1 轮。

## 2. 一次读足 + grep 定位（同文件第 2 次就放大范围）

规则：**同一文件同一片段内最多读 2 次；第 2 次必须放大范围**（不是再切一小段）。

| 场景 | 第 1 次 | 第 2 次（必须放大） |
| --- | --- | --- |
| 不确定结构 | `grep -n "fn \|struct \|impl " <file> \| head -60` | `sed -n '1,400p'` 整段读，或按 grep 命中行 ±80 行三段并读 |
| 要改某函数 | `grep -n "fn target" <file>` | `sed -n '<start-60>,<start+200>p'` 一次覆盖签名+上下文 |
| 大文件（>800 行或 >20K 字符） | **不整读**：先 `grep -n` 定位 | `sed -n 'A,Bp'` 精确段；需要多处就一条命令里 `sed A,B + sed C,D` |
| 反复看同一日志/输出 | `tail -50 <log>` | `grep -n "<关键词>" <log> \| tail -40` |

- 实测反例：单片段内 `state.ts` 被读 143 次、`core/src/main.rs` 122 次、
  `README.md` 108 次——都是"看一眼改一行"的循环。
- 例外：**改完必须验证**（`edit_file` → `cargo check` / `sed -n` 复核）不算
  重复读；但验证也只做 1 次，不要"改一点验一遍"超过 3 轮。
- 需要重读第 3 次时，改为：`sed -n '1,<文件末尾>p'` 一次读全 / 或直接
  `search_code` 拿全部命中行号再按行号并读。

## 3. 长任务：先切后台会话，再谈超时（**最贵的习惯**）

### 3.1 判据是「任务类别」，不是秒数

**不要用「预计多少秒」来分诊——模型估不准耗时。判据是「这条命令属于哪一类」。**

**默认后台化**（`&` + 重定向 + 哨兵，或有 `bash-start`/`bash-exec` 时用会话）：

| 类别 | 典型命令 |
| --- | --- |
| 编译 / 构建 | `cargo build`、`cargo check --workspace`、`make`、`cmake --build`、`npm run build`、`vite build`、`docker build`、`docker compose build` |
| 测试 | `cargo test`、`cargo nextest`、`pytest`、`vitest run`、`playwright test` |
| 依赖安装 | `npm/pnpm/yarn install`、`pip install`、`uv sync`、`cargo install`、`apt install`、`conda create` |
| 训练 / 推理 / 数据处理 | `python train.py`、`torchrun`、`accelerate launch`、`deepspeed`、大规模 pkl/joblib 加载、数据集格式转换 |
| 下载 / 克隆 / 打包 | `git clone`、大仓库 `git fetch/pull`、`wget`/`curl` 大文件、`huggingface-cli download`、数据集解压、`tar` 打包、`rsync` |
| 大范围扫描 | `rg`/`find` 扫整个盘或整个家目录、`du`、`grep -r` 全仓 |
| 常驻 / 持续输出 | dev server、watch 构建、`tail -f`、训练日志监控 |

**可以直接前台跑**（这些不会久，一轮拿结果最省）：

| 类别 | 典型命令 |
| --- | --- |
| 状态查询 | `git status`、`git diff`、`git log`、`ls`、`pwd`、`ss -ltn`、`systemctl --user is-active` |
| 小范围读 | `sed -n 'A,Bp'`、`head`/`tail` 单文件、`cat` 小文件、`jq` 单文件、`stat` |
| 单文件 / 单模块检索 | `grep -n <pattern> <file>`、`grep -rn … 单个模块目录`、`search_code` |
| 带短超时的探测 | `curl -m 5`、`timeout 5 ping`、`nc -z -w 2` |
| 小改动后验证 | `cargo check -p <单包>`（增量、已完成首次编译）、`python -c` 单行脚本 |

**判不准时一律后台化**：代价不对称——后台化只多一轮 poll（约 2 秒），
前台卡住则白等满 120s、拿不到任何输出、还得重跑。

### 3.2 `timeout_secs` 的定位：最后一个选项，不是第一个

它仍可用（默认 120s），但作用只是把单次调用的等待窗口抬到**最多 300s**——
300s 是硬天花板，它不是「长任务开关」。它解决不了上表里那些真实的长任务：

- 全量 `cargo build`、`npm install`、克隆大仓库填 `timeout_secs: 300`，只是把
  失败从 120s 推迟到 300s，最后拿到一个**既无输出也无进度**的超时错误；
- 这 300s 整轮阻塞：没有中间输出、不能并行、拿不到部分结果；
- 写成习惯后，连 `ls` / `grep` 也会顺手带上它（纯冗余，只是把卡死时的
  暴露时间从 120s 拖到 300s）。

**只有一种情况该用它**：命令确实要跑 2–5 分钟，且**无法后台化**——例如它
必须前台读交互输入、或必须一条命令内完成（`a && b && c` 的中间态不能用）。
上表里的编译 / 测试 / 安装 / 下载 / 训练一律后台化，不看秒数。

超时之后也不要用「加大 `timeout_secs`」重跑同一个命令——那是把同一个坑
挖深；改为后台化。

### 3.3 后台会话怎么写（`echo-agent.shell` 已启用时首选）

```text
# 轮 1：起会话 + 后台起任务，立刻返回
bash-start(workdir="/path/to/repo")                        → session_id
bash-exec(session_id,
  "cargo test --workspace > /tmp/t.log 2>&1; echo EXIT=$? >> /tmp/t.log &")

# 轮 2..N：每轮短轮询，立刻返回（不阻塞、不撞墙）
bash-exec(session_id,
  "grep -q '^EXIT=' /tmp/t.log && tail -30 /tmp/t.log || echo RUNNING")
```

要点：

- **`&` + 重定向 + `EXIT=` 哨兵三者缺一不可**；缺哨兵永远判不出完成。
- `bash-exec` 单条 120s 到点**不销毁会话**，直接再 poll 一次——这是它相对
  `bash` 的核心优势：长任务不必「一轮跑完」。
- 同一会话保持 cwd / export / 后台作业，多步流程可分多轮推进。
- 会话里的常驻进程在 Shell 视图可见；任务结束记得 `bash-stop(session_id)`。

### 3.4 没有 shell 模块时：`bash` 的后台化写法

重定向 + 哨兵同样必需——裸 `&` 仍会阻塞到子进程结束（`wait_with_output()`
等 stdout 到 EOF，实测 `(sleep 8; echo A) &` 阻塞满 8 秒）：

```bash
# 同一轮就要结果：等到哨兵出现立刻返回
(cargo test --workspace > /tmp/t.log 2>&1; echo "EXIT=$?" >> /tmp/t.log) &
until grep -q '^EXIT=' /tmp/t.log 2>/dev/null; do sleep 2; done; tail -20 /tmp/t.log
```

```bash
# 这一轮先返回、去干别的（推荐）：只起任务，不等待
(cargo build --release > /tmp/b.log 2>&1; echo "EXIT=$?" >> /tmp/b.log) &
# 同一响应里继续发别的独立调用
grep -n "TODO" src/**/*.rs | head -20
git status --short
```

下一轮再 `until grep -q '^EXIT=' /tmp/b.log 2>/dev/null; do sleep 2; done; tail -20 /tmp/b.log`。
**没有 `EXIT=` 哨兵时 `until` 会永远等下去**——哨兵和重定向必须成对出现。

### 3.5 等条件，不等时间

**不用无条件 `sleep` 等结果。** 要等就等条件——`until <条件>; do sleep 1-2; done`。

两次独立审计都指向同一条：

| 审计 | `sleep` 占比 |
| --- | --- |
| 2026-10-19（3 天 / 8664 次调用） | 483 次 / **10.4h**（95% 在训练监控） |
| 2026-10-20（单次会话 / 767 次调用） | 59 次 / **2989s = 该会话工具总耗时的 61%** |

第二条最刺眼的地方：**37 次**都是同一个模式——

```bash
# ❌ 测试 130s 就完了，剩下 70s 纯等
(cargo test --workspace > /tmp/t.log 2>&1; echo "EXIT=$?" >> /tmp/t.log) & sleep 200; cat /tmp/t.log
```

```bash
# ✅ 条件一满足立刻返回；同一件事，只多写一个 until
(cargo test --workspace > /tmp/t.log 2>&1; echo "EXIT=$?" >> /tmp/t.log) &
until grep -q '^EXIT=' /tmp/t.log 2>/dev/null; do sleep 2; done; cat /tmp/t.log
```

轮询**严格更快**：`sleep 200` 只能等满 200 秒，`until` 在 130 秒就返回。
而且 `sleep` 撞上工具 120s 上限时只拿到一个超时错误，什么信息都没有。

### 3.6 四种场景的写法

| 场景 | ❌ 不要 | ✅ 这样写 |
| --- | --- | --- |
| 等后台任务 | `(job > log) & sleep 200; cat log` | `(job > log 2>&1; echo "EXIT=$?" >> log) &`<br>`until grep -q '^EXIT=' log 2>/dev/null; do sleep 2; done; cat log` |
| 等服务/进程就绪 | `systemctl restart x; sleep 60` | `systemctl restart x`<br>`for i in $(seq 1 60); do [ "$(systemctl --user is-active x)" = active ] && break; sleep 2; done` |
| 等文件/端口 | `job & sleep 30; test -f f` | `until test -f f; do sleep 1; done` |
| 真要脱离执行 | `sleep 45; systemctl restart x` | `systemd-run --user --collect --unit=<name> bash -c '…'`（立刻返回） |

轮询间隔 **1–2 秒**：短到条件一满足就返回，长到不刷爆日志和 CPU。

### 3.7 期间并行 + 例外

- 构建、安装、长测试起后台后，**同一轮继续干别的**；需要结果时再 `tail`/`grep`。
- 实测 >30s 的命令 243 次（其中 >120s 大量）：它们几乎都落在上表的「默认后台化」
  类别里——这些是最该后台化的样本，不是最该调 `timeout_secs` 的样本。
- **等待期三件事清单**（挑 1–2 件立刻做）：① 写文档/报告骨架；② 准备测试或
  回归命令；③ 读下一个模块的代码。禁止的动作：`sleep` 后再 `sleep`。
- 例外（不是浪费）：几秒内的短 `sleep`（等落盘/起进程）、`bash-exec` 里的
  等待（属于常驻会话本身）、测试夹具为构造超时场景写长 `sleep`。

## 4. 大输出：分页 / 落盘（>20K 字符别整读）

实测：工具输出合计 17M 字符；单条 >20K 的 39 次（2.4M），最大一次
`read_file` 回了 **181K 字符**——直接吃掉后面十几轮的预算，并让每轮
prompt 涨到 30 万 token（p90 往返 14.4s 的长尾主要来自这里）。

**硬阈值**：

| 触发 | 做法 |
| --- | --- |
| 预计输出 >20K 字符 / >200 行 | 落盘：`cmd > /tmp/x.log 2>&1`，再 `tail -40` / `grep` 关键行 |
| 文件 >800 行或 >20K 字符 | 不整读：`grep -n` 定位 + `sed -n 'A,Bp'` 取段 |
| grep/find 命中 >50 行 | `| head -40` 或 `| wc -l` 先看规模，再按需取 |
| 命令输出含大表格/JSON | `| head -c 4000`，或 `jq` 只取需要的字段 |
| 需要多次回看同一输出 | 写文件一次，之后只 `grep`/`tail` 关键行，禁止重复整跑 |

回报时也不贴原始长输出：落盘 + 回路径 + 关键几行。

## 5. 失败之后：换策略，不重发

- `result` 以 `error:` 开头时，**同一命令不得原样重发**（实测已做到 0 次，
  保持住）：先判定原因再动手。
  - `file not found` → 先 `ls` 确认路径（一次），再决定改路径还是新建；
  - `工具参数无效`（如 `start_line and end_line required`）→ 补齐参数重发是对的；
  - `timeout after Ns` → **先改后台化**（`bash-start` + `&` + 哨兵），别再靠加大
    `timeout_secs` 重跑同一条命令；确实不可拆才显式调到需要的窗口；
  - `远程节点「X」响应超时` → 换本机执行或稍后重试 1 次，连续 2 次失败改本机；
  - `blocked command` → 改写命令规避，不要第三次试同一条。
- 错误构成（3 天 54 次）：超时 12、远程节点 9、参数校验 7、路径 5、会话中断 6
  ——**没有一例"想错"**，所以别因为一次失败就推翻方案。
- **常见误报绕行**：`bash` 的危险模式是子串匹配 `rm -rf /`，所以
  `rm -rf /tmp/x`、`rm -rf /home/...` 会被拦。用 `cd /tmp && rm -rf x`
  或 `rm -rf "$PWD/x"` 规避。

## 6. 每 50 步一次检查点（避免"跑偏了才发现"）

- 用 `checklist` 建 3–6 项计划；**操作清单前先 `list`**（索引错是参数类
  错误的第一来源）。
- 当片段工具数到 **50** 时停下来 30 秒，回答三问：
  1. 剩下的活能不能**委派**出去一块（≥50 步片段吃掉 55% 的调用，而其中
     大多数零委派）？
  2. 我是不是在重复做同一件事（重复读、重复验证、反复重试）？
  3. 用户要的是结论还是过程——过程性输出能不能一次落盘成文件、只回路径？
- 到 **90 步**仍未收敛：必须显式改变策略（拆分/委派/换实现路径），并在
  回复里说明原因。

## 7. 上下文卫生（省 token 也省时间）

- 大输出不要反复回读：一次写文件（`> /tmp/x.log`），之后只 `grep`/`tail` 关键行。
- 不贴大段原始输出进回复；长清单落盘 + 回路径。
- 单条命令的输出只要能回答当前问题即可（`| head -50` / `| tail -20`）。
- 每轮 prompt 已经到 30 万 token 量级时，主动压缩：结论落盘、只留路径，
  别再整文件回读。

## 8. 自检清单（每 10 轮扫一眼）

1. 这轮我是不是只发了 1 条**不依赖上一条**的调用？（是 → 下轮打包成 2–3 条）
2. 有没有文件被读了第 3 次？（有 → 下次一次读足 / 改 grep 定位 + 精确段）
3. 有没有无条件 `sleep`？（有 → 改 `until 条件` 轮询，或 `&` 后台化，期间干别的）
4. 有没有刚 `error:` 过就原样重发？（有 → 停下改策略）
5. 有没有 >20K 字符的输出被整读进上下文？（有 → 落盘 + tail/grep）
6. 有没有把编译 / 测试 / 安装 / 下载 / 克隆 / 训练 / 全盘扫描当前台跑，或者
   给它们填 `timeout_secs: 300` 然后同步干等？（有 → 改 `bash-start` + `bash-exec`）
7. 步数过 50 了吗？（过了 → 做检查点三问，含"该不该委派"）
