---
id: selfupdate
title: "自更新模块"
group: Core
x: 3708
y: 216
---

# 自更新模块

> **定位**：`echo-agent.selfupdate` **这个模块本身**——两条命令的归属、为什么它不再是「宿主保留命令」、以及部署形状（unit 名、状态文件）为什么落在模块里。更新流程本身（何时触发、怎么回滚、失败怎么恢复）见 [部署与自更新](../ops/deploy.md)。读者：改更新链路、或排查「点更新没反应」的开发者。

## 它曾经必然是坏的

此前这两条命令在 `HOST_RESERVED_COMMANDS` 里，注释是「模块不得接管的自我管理面」。结果是**命令泵在分发前就跳过模块**，而实现只在「模块分发 → 命令处理器」这条路上——两条命令必失败，文案还误报「联邦模块未启用」。

保留名单的真正理由是「别让第三方模块劫持自更新」。这个理由用**内置模块**满足更准确：

- 它编译进本二进制、启动期即挂载、id 写死在 `MODULE_ID`
- 第三方模块要抢必须先改代码再重新编译——那已经越过了「名单」能防的边界
- 「谁能认领命令」本来就不该由一张 8 条命令的名单守卫（`SendMessage` 也没被守卫）

于是名单里的这两条被移除，改由模块认领。装配期还有一条守卫兜底：任何模块若声明了仍属宿主保留的命令，`reject_reserved_commands` 直接让**启动失败**——「声明了但收不到」这类死代码不可能再溜进来。

## 身份与贡献

| 项 | 值 |
|---|---|
| 模块 id | `echo-agent.selfupdate` |
| kind | `orchestration` |
| 名称 / 路由 | 「自更新」/ 路由 `selfupdate` |
| 来源 / 运行形态 | 编译内置 / 进程内 |
| crate | `source/modules/agent-module-self-update`（3 文件，约 490 行） |

| 贡献 | 内容 |
|---|---|
| 命令域 × 2 | `RequestSelfUpdate` / `RequestSelfUpdateStatus` |
| 服务依赖 | `echo-agent.host.selfupdate.events`（事件出口，唯一需要的宿主能力） |
| 服务提供 | 无 |

### kind 为什么是 `orchestration` 而不是 `management`

Panel 会把 `management` 类模块放进**人格级**「启用模块」勾选区。而自更新按人格启停是个**假开关**——挂载是进程级的，命令分发也不看人格白名单。标成 `orchestration` 后它只出现在进程级开关里，勾选语义与实现行为一致。

## 两条命令做什么

```
RequestSelfUpdate        → start_update()  → systemctl --user start <unit>
RequestSelfUpdateStatus  → read_status()   → ~/.local/state/echo-agent-core/update-status
```

两者都只经 `SelfUpdateHost::emit` 说话，**不碰任何宿主类型**——这是 S6「模块零捕获」的直接体现：`module(version)` 零参数，唯一需要的宿主能力在 `mount` 时从服务表解析。

## 部署形状归模块

unit 名与状态文件布局写在本 crate 里，不上移到宿主。判据：**换了部署方式（systemd → 容器编排）要改的是这组常量，而不是整个宿主**。相应地，宿主的 `install.sh` / `update.sh` 要与之保持一致——`scripts/update.sh` 的 `expected_ids` 是模块感知校验的核对清单，与 `BUILTIN_MODULE_IDS` 由单测守护。

## 边界

- 它**不执行**更新：真正的构建—替换—重启在一个脱离本进程的 oneshot 单元里（否则替换自己的二进制会把执行者一起干掉）
- 它**不判断**要不要更新：版本比较与触发器在 `update.sh` / 自更新单元里；模块只负责「人点了按钮 → 启动那个单元」和「现在到哪一步了」
- 失败恢复与回滚见 [部署与自更新](../ops/deploy.md)
