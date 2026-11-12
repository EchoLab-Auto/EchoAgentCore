---
id: prodoc
title: "ProDoc 文档群模块"
group: Core
x: 4160
y: 1440
---

# ProDoc 文档群模块

> **定位**：`echo-agent.prodoc` **这个模块本身**——它负责把一棵 Markdown 目录读成文档图所需的文件映射，交给 Panel 的画布渲染。渲染、布局、搜索在浏览器里（ProDoc 的 `DocGraphViewer`），**读盘在 Core**。读者：要把文档群接进面板、或排查「文档图画布空白 / 少节点」的开发者。

## 为什么读盘必须在 Core

ProDoc 原本是独立 CLI（`echo-prodoc view ./document`），自带一个 Vite dev server 负责三件事：读盘成 `files` 映射、写盘（带冲突合并）、热更新推送。嵌进 Panel 时这三件事必须换通道，而**读盘归 Core**：

Panel 是独立进程，没有文件系统权限模型，也不该为了"看个文档"去开一个文件接口。文档根是 Core 侧的资源，边界就该在 Core 侧定。

| 环节 | 位置 |
|---|---|
| 扫描目录 → `相对路径 → 原文` | 本模块（`bundle.rs`） |
| 路径沙箱（根内 / `.md` / 父目录 realpath） | 本模块（`paths.rs`） |
| 渲染文档图、布局、搜索、编辑 | 浏览器（`DocGraphViewer`） |
| 面板事件投递 | 宿主（`BackendEvent::ModuleEvent` 透传） |

## 身份

| 项 | 值 |
|---|---|
| 模块 id | `echo-agent.prodoc` |
| kind | `tool`（persona 白名单参与） |
| 名称 / 路由 | 「ProDoc 文档群」/ `prodoc` |
| 实现位置 | `source/modules/echo-module-prodoc`（Core 仓内，path 依赖） |

## 上限：与 CLI 唯一的刻意差异（扫描侧）

| 项 | CLI（`server.ts`） | 本模块 |
|---|---|---|
| 单文件 | 无上限 | 1 MB |
| 篇数 | 无上限 | 2000 |
| 总量 | 无上限 | 32 MB |
| 跳过目录 | 5 个固定名 | 同 5 个（`node_modules` `.git` `.cache` `dist` `build`）+ `target` + 隐藏目录 |

CLI 是一次性页面加载，超量顶多慢；本模块走 WS 推送，一个几千文件的目录能瞬间生成几十 MB 的事件消息，把面板的模块事件环形缓冲（200 条）撑爆。

超限**明确报错并列出被跳过的文件**（`Bundle.skipped`），绝不静默截断——截断的文档图会缺节点，而作者看不出是"被截了"还是"本来就没写"。

## 事件协议

模块发 `prodoc.bundle`（快照）与 `prodoc.error`（失败）。宿主不认识这两个名字，于是原样包成 `BackendEvent::ModuleEvent` 推给面板，扩展在 `state.moduleEvents` 里按名字取。

这条路**不需要**给线协议加变体，也就不需要改 Panel 的类型定义——是"模块事件通道"这个既有通用能力的正当用法。

| 事件 | 载荷 |
|---|---|
| `prodoc.bundle` | `{ team_id, root, files, skipped, stats }` |
| `prodoc.error` | `{ team_id, root, error }` |

命令只回执统计（`{root, documents, bytes, truncated}`），内容走事件：一份快照可能几百 KB，塞进命令回执会和面板的轮询/重放逻辑缠在一起。

## 命令域

| 命令 | 语义 | 只读 |
|---|---|---|
| `RequestProDocBundle` | 扫描文档根并推快照 | 是 |
| `BrowseProDocDirs` | 目录选择器逐层浏览（只列子目录） | 是 |
| `ProDocRoots` | 可打开的根列表 + 生效的上限 | 是 |
| `ReadProDocFile` | 读单文件原文（冲突对照 / 合并基准） | 是 |
| `SaveProDocFile` | 保存单文件（带 `base` 冲突检测，原子写） | 否 |
| `DeleteProDocFile` | 删除单文件（`base` 不匹配则取消） | 否 |
| `ProDocCheck` | 文档图检查（断链 / 自连 / 重复 id / 环 / 孤立…） | 是 |

`ReadProDocFile` / `ProDocCheck` 在 `PANEL_READONLY_COMMANDS` 里，`Save`/`Delete` **不在**——只读面板能看不能改。

## 配置

```toml
[module.config]
# 允许打开的文档根（绝对路径）。空 = 只允许 HOME。
roots = ["/home/me/docs", "/srv/wiki"]
max_file_bytes = 1048576
max_files = 2000
max_total_bytes = 33554432
```

`roots` **刻意不用"任意绝对路径"**：那等于给面板开了一个读全盘 `.md` 的口子。配置为空时回退 HOME——刚装上还没配时，面板至少能让人从家目录往下走。

## 写盘：冲突合并为什么在前端

保存走的是与 CLI **逐条对齐**的语义（`write.rs`）：

- `base` 缺省 = 无条件覆盖（新建文件走这条）；带 `base` 时整篇**内容全等**比对，不引入版本号 / mtime / hash；
- 提交内容与磁盘已一致 → 幂等成功（网络重试不会变成冲突）；
- 磁盘变了 → 冲突，**不写盘**，回执带 `disk`（磁盘当前内容）；
- 带 `base` 但文件已被删 → 冲突且不重建（重建等于撤销别人的删除）；
- 删除同理：`base` 不匹配即取消删除，删除**不做合并**；
- 写盘用同目录临时文件 + `rename`（直接 `write` 在中断时会把参数区截断，下次扫描就会把它当成无参数区的文件、节点 id 从文件名推断，连线全断）。

冲突后的合并**留在前端**：CLI 用的是 `prodoc-core/merge.ts` 的 265 行 hunk 级 diff3，在 Rust 里复刻必然与它语义分叉，结果是"面板说合并成功、命令行说冲突"。所以前端拿到 `disk` 后跑 vendored 的同一份 `merge3`，干净就以 `base = disk` 重发一次；有重叠才报冲突块数交给用户。两次往返换来两端行为一致，是刻意的取舍。

## 文档图检查（`prodoc_check` 工具 / `ProDocCheck` 命令）

检查在 Rust 里重新实现了一遍**图结构**那部分（`check.rs`），因为 agent 不能起一个
Node 进程去跑 CLI。复刻就有分叉风险，所以：

1. **边界是刻意划的**——本模块覆盖断链 / 自连 / 重复 id / 环 / 孤立 / 空正文 /
   超长 / 缺参数区 / 单轴坐标；group 相关两项（多 group 声明 / 显式几何冲突）不
   覆盖，那是纯布局外观问题。需要那两项请跑 CLI。
2. **对照过 CLI**：`tests/cli_parity.rs` 留了一个手动跑的对照入口（`#[ignore]`，
   需要 `PRODOC_FIXTURE`），文件头记录了 2026-11 的实测结果——10 篇文档 / 6 条
   连线 / 3 个错误 / 7 条警告，两边逐项一致。

模型侧入口是宿主注册的 `prodoc_check` 工具（`echo-module-loop` 的
`agent/prodoc_tool.rs`，与 `workspace` 工具同一种代理形态）：工具在宿主注册表里，
检查逻辑在模块，中间只传 `team_id` 与 `root`。面板侧是画布工具栏的「检查」按钮，
报告按错误 / 警告 / 提示分组展示。

## 尚未实现

- **热更新推送**：CLI 用 Vite watcher 把整份 `files` 推给页面，本模块没有文件监听——面板靠手动「刷新」重拉快照。加监听要引 `notify` 依赖并处理去抖/单飞，属后续。
- **组件侧的磁盘对照**：`DocGraphViewer` 的冲突面板硬编码 `fetch('/__prodoc_api/raw')` 读磁盘原文，面板里没有这个端点，所以"磁盘一侧"是空的（不影响保存与合并；"采用我的版本"仍按强制覆盖走）。补齐要给组件加 `readHandler` prop，属上游改动。
