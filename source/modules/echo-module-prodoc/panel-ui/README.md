# ProDoc Panel UI 扩展

`echo-agent.prodoc` 的前端扩展：一个「文档」入口 + 一个 overlay，把 Core 扫出来的
文档群交给 ProDoc 的 `DocGraphViewer` 渲染成文档图画布。

## 安装

```bash
# Panel 安装/更新时带上本目录（可重复给多个扩展）
PANEL_UI_EXTENSIONS="/path/to/crate-workspace/panel-ui:/path/to/echo-module-prodoc/panel-ui" \
  bash ~/.local/libexec/echo-agent-panel/update.sh --source /path/to/EchoAgentPanel
```

装好后 `~/.local/state/echo-agent-panel/ui-extensions` 会记住这份清单，后续更新自动沿用。

## 构建与测试

从 Panel 的 `web/` 目录：

```bash
PANEL_UI_EXTENSIONS=/abs/path/to/echo-module-prodoc/panel-ui npm run build
PANEL_UI_EXTENSIONS=/abs/path/to/echo-module-prodoc/panel-ui npm test
```

## 数据流

```text
扩展 sendCommand(RequestProDocBundle)
       └─► Core 命令通道 ─► prodoc 模块扫描目录
                                  │
扩展读 state.moduleEvents ◄───────┘  ModuleEvent{prodoc.bundle} ─► DocGraphViewer
```

三条命令（`RequestProDocBundle` / `BrowseProDocDirs` / `ProDocRoots`）的**输出全部走模块
事件**：宿主 `apply_command` 只认 `Err`（转成错误事件），`Ok(value)` 被直接丢弃。这条
如果搞错，表现是"命令发了、界面永远转圈"，而且日志一片安静。

## vendored 渲染器

`vendor/prodoc-renderer/` 是 ProDoc 仓 `packages/prodoc-renderer/dist` 的产物
（`index.js` + `index.css` + 一份精简 `index.d.ts`），**不是** npm 依赖：

- 面板的扩展构建把外部扩展的裸包名 import 一律重定向到 `web/src/extension-api.ts`
  再解析（`extension-build.ts` 的 `resolveId`），所以 `import '@prodoc/renderer'` 在
  `web/node_modules` 里找不到就会失败；
- vendored 版本里的 `from '@echolab-auto/ui-frame'` / `from 'vue'` 会被解析到**面板自己
  那一份**——这正是我们要的：两份 ui-frame 会让 ThemeProvider 上下文与 CSS 变量打架。

升级步骤：在 ProDoc 仓 `npm run build`，把 `dist/index.js`、`dist/index.css` 覆盖过来，
删掉 `index.js` 末尾的 `//# sourceMappingURL=` 行（否则 vitest 会抱怨找不到 .map），
再跑一遍 `npm run build` 与 `npm test`。

## P1 只读

`ProDocPanel.vue` 显式给了一个**总是拒绝**的 `saveHandler`：ProDoc 自带的编辑入口
需要写处理器，不给的话点保存只会得到"未提供写处理器"——那对用户是噪音。所以这里
把原因说清楚（"当前只读，写盘属 P2"），而不是让编辑按钮看起来能用。假成功更糟：
用户以为存了，刷新后改动消失。
