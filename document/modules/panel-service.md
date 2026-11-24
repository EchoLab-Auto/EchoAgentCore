---
id: panel-service
title: "面板服务"
group: Core
x: 2448
y: 384
---

# 面板服务（Panel Service）

> **定位**：面板按**服务名**向 Core 请求数据的那条通道——模块怎么登记、Core 怎么分发、面板怎么配对。读者：要给面板新增一个读接口的模块作者，或排查「面板点开没数据」的开发者。具体模块的读方法见各自文档（如 [会话模块](./session.md) 的 `echo-agent.sessions`）。

## 它解决的问题：面板要数据，但只有广播

在它之前，面板要读某个模块的数据只有两条路：

| 路 | 形状 | 缺什么 |
|---|---|---|
| 广播 | 宿主在变化时发事件，面板累积成 `state` | **没有"现在给我一份"的入口**。进程重启后磁盘上有数据、但没有任何变更事件，面板就空着 |
| `InvokeModuleOp` | 按**模块 id** 点方法 | 面板必须硬编码实现者是谁。模块改名、或同一件事换个模块实现，面板就断 |

2026-11 的现场是第一条：会话列表靠 `SessionUpdated` 累积，Core 重启后侧边栏空着，直到用户手动动了一下工作区触发一次广播才自愈。

本通道提供第三条：**按服务名寻址**。名字是稳定的语义名（`echo-agent.sessions` 而不是 `agent-module-session`），面板问"谁会列会话"，提供方可以换。

## 三层，方向各不相同

```text
模块 mount 时                     每次面板请求
──────────────                    ──────────────
PanelServiceRegistrar.register()  InvokeService { service, method, payload, request_id }
        ↓                                  ↓
   注册表（Core 组合根持有）  ←──────  命令泵 → PanelServiceInvoker.invoke()
        ↓                                  ↓
   Arc<dyn PanelService>              ServiceInvokeResult { ...ok, data, error }
```

| 角色 | 谁 | 时机 |
|---|---|---|
| 注册表（提供方） | Core 组合根 `provide` | **任何模块 mount 之前** |
| 登记 | 各模块在自己的 `mount` 里 `register` | 挂载时；卸载时由 `Disposer` 撤销 |
| 分发 | 命令泵（`process_commands::try_handle`） | 每条 `InvokeService` |

注册面与分发面是**两个 trait**、两个键（`echo-agent.host.panel_service_registrar` / `..._invoker`）：模块拿到的句柄只能登记，不能遍历别人的服务。否则"服务"就退化成一张全局函数表，与直接互相依赖没有区别。

## 契约（`echo_defs::panel_service`）

```rust
#[async_trait]
pub trait PanelService: Send + Sync {
    fn name(&self) -> &'static str;                  // "echo-agent.sessions"
    fn methods(&self) -> &'static [&'static str];    // ["list"]
    async fn invoke(&self, method: &str, payload: Value) -> Result<Value, String>;
}
```

**服务名用复数名词**（`echo-agent.sessions`），方法名单独一级。**不要**写成 `echo-agent.session.list`——那会把两级压成一级，加第二个方法时无路可走。

载荷是 `serde_json::Value`：定义层不该认识任何模块的数据类型（否则 `echo-defs` 会依赖所有模块）。形状由服务自己约定并写进方法文档，注册表只路由、不解释。

## 线协议

| 方向 | 变体 | 关键字段 |
|---|---|---|
| 命令 | `InvokeService` | `service` / `method` / `payload` / **`request_id`** |
| 事件 | `ServiceInvokeResult` | 回显三者 + `ok` / `data` / `error` |

`request_id` **内建在协议里**而不是塞进 `payload`，两个理由：

1. 一个服务有多个方法，靠"事件名对应命令名"的映射表（`pending.ts` 的 `REQUEST_RESPONSES`）表达不了；
2. 失败回执**也带** id。`InvokeModuleOp` 那边是模块自己在 payload 里塞 id，失败回执里没有——2026-11 排查 `workspace.deleteConversation` 时只能靠超时猜。

## 必回一条回执

命令泵对**每一条** `InvokeService` 都回 `ServiceInvokeResult`，三层拒绝各给可读原因：

| 情况 | 回执内容 |
|---|---|
| 服务名没登记 | `面板服务 X 不存在；已登记：a, b, c` |
| 服务在、方法没声明 | `面板服务 X 未声明方法 m；可用方法：...` |
| 服务自己失败 | 原样透传（模块的领域原因，注册表不加工） |
| 服务表未接线（嵌入式部署） | `面板服务通道未接线（宿主未装服务注册表…）` |

前三层由注册表负责，第四层由 `ProcessCommandHost::panel_services == None` 负责。都不静默——静默会让面板一路等到超时，排查时也看不出"是没发出去、还是发了没人接"。

`command_clearance` 把 `InvokeService` 归 `Frontend`（与 `InvokeModuleOp` 同类）：模块只把只读能力登记成服务，写操作走各自模块的私有方法，所以作用面比通用调用更窄；但仍然是"任意服务任意方法"，模型不可控它。

## 登记语义

- **同名覆盖**而不是冲突失败：模块重启后会把上一轮的服务重新登记一次，那是常态。
- 返回的 `Disposer` 只撤销**自己那一代**（内部带 `generation`）——"模块 A 重启 → 新服务登记 → 旧 disposer 晚到析构"不会把新的顶掉（`Ctx::register` 同一手法）。
- 空服务名 / 空方法名 / 重复方法名在登记时就 `Err`：面板侧的表现是"服务存在但方法 404"，排查要翻三个 crate，越早报越好。

## 面板侧

`web/src/panel-services.ts`：`invokeService(service, method, payload, teamId?)` 返回 `Promise<data>`。

- 配对键是 `request_id`（不是事件名），所以同一方法的并发调用不会串。
- **拒发即失败**：`sendCommand` 返回 `false`（多上游解析不出目标 Core）时当场 reject，不登记等待——登记了没人销账，用户只能等超时。
- 超时 15s 只是兜底，提示文案要说明"Core 侧必回，超时通常意味着命令没发出去或连接已断"。
- 断连时 `resetServiceCalls()` **reject** 全部在飞调用（不是静默清表）：静默会让调用方的 `await` 挂住，视图加载态永远收不掉。

回执在 `store.ts::dispatch` 里最先被认领（`consumeServiceResult`）：命中即消费，返回值归调用方解释，壳不把它塞进任何 `state` 字段——那是模块的数据，壳不认识。

## 姊妹接缝：面板 UI 贡献（`echo-agent.host.panel_ui_registrar`）

服务通道解决"面板要读数据"，UI 贡献解决"面板要显示模块界面"。两者同源——
都是"模块登记、Core 保管、面板消费"，但载荷不同：

| | 面板服务 | 面板 UI 贡献 |
|---|---|---|
| 载荷 | 一个可调用的 trait 对象 | 一份描述（bundle 路径 + SDK 版本） |
| 面板侧动作 | `InvokeService` 调方法，等回执 | `import()` 加载 ES bundle |
| 契约 | `echo_defs::panel_service` | `echo_defs::panel_ui` |
| 键 | `...panel_service_registrar` / `..._invoker` | `...panel_ui_registrar` |

清单由组合根自己登记成面板服务 `echo-agent.panel-ui`（方法 `list`）：清单是
注册表的**投影**，不该寄生在任何模块上——否则"列出 UI 模块"会依赖那个模块是否
挂载，一个模块没装面板就看不到别人也有界面。

### 三条硬约束（登记时校验，不是文档约定）

| 约束 | 拦住的后果 |
|---|---|
| `bundle` 必须是站点相对路径（无协议 / 无前导 `/` / 无 `..`） | 面板变成任意脚本加载器——一个被投毒的模块描述就能在用户浏览器里执行外部代码 |
| `sdk_version` 非空，加载前逐字比对 | 运行期加载的 API 用错只在用户浏览器里崩；版本握手是唯一拦截点 |
| 产物文件名不带 hash（由模块构建约定） | 面板按固定路径 `import()`，名字一变所有已构建的扩展失联 |

### 进程外模块怎么参与

`kind = "stdio"` / `"dylib"` 的模块拿不到 `Ctx`，无法直接调注册表，所以经协议
`Register` 的 `Contribution::Custom { custom_kind: "panel_ui", name: <id>,
payload: { name, bundle, sdk_version } }` 上报。`panel_ui` 是本宿主的词汇，
框架只搬运、不解释——所以框架仓不需要为它加变体。

## 边界

- 它**不是**模块间通信通道：模块之间要互相调用走 `ServiceKey` + `Ctx::provide`（进程内），本通道的消费者只有命令泵。
- 它**不做**写操作约定：写仍归各模块的私有方法（`InvokeModuleOp`）或专用命令。服务的定位是"面板要读的领域快照"。
- 它**不排序 / 不裁剪**返回值：那是服务的领域规则（如会话列表要不要跨人格）。
