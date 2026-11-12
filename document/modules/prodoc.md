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

## 上限：与 CLI 唯一的刻意差异

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

| 命令 | 语义 |
|---|---|
| `RequestProDocBundle` | 扫描文档根并推快照 |
| `BrowseProDocDirs` | 目录选择器逐层浏览（只列子目录） |
| `ProDocRoots` | 可打开的根列表 + 生效的上限 |

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

## 与 ProDoc CLI 的演进关系

P1 只读。写盘（带 `base` 的三路合并）、热更新推送仍属后续：编辑能力要先解决"面板后端做不做 diff3"——ProDoc 的合并在 `prodoc-core/merge.ts`（265 行 hunk 级 diff3），在 Rust 里复刻保证不了与 CLI 语义一致，倾向留在前端、让后端只做"读原文 + 带 base 校验的原子写"。
