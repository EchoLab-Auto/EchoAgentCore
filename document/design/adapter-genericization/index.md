---
id: adapter-genericization
title: "适配器泛化（设计系列）"
group: 规划
x: 1440
y: 3320
link: ["gen-01a | 01a Core UI 描述符 | b>l", "gen-01b | 01b 线协议去号码化 | l>t", "gen-02a | 02a Panel 协议层 | l>t", "gen-02b | 02b Panel 重接线", "gen-03a | 03a 渲染器规范", "gen-03b | 03b 迁移与删除 | r>t", "gen-05 | 05 对账表 | r>t", "gen-06 | 06 进度台账 | r>t", "gen-evidence-00 | 证据 · 冻结接口 | r>t"]
---

# 适配器泛化（设计系列）

> **定位**：把「平台专属命令 / 面板 / 工具」拆成通用适配器契约那一轮改造的完整设计包——分册设计（01 / 02 / 03）、证据包、跨文档对账表与执行进度台账。读者：维护适配器契约、或要接手这套抽象的人。落地结果看 [适配器插件化设计](../instance-plugin.md) 与 [插件开发指南](../../plugins/plugin-authoring.md)，机制现状看 [QQ 适配器门控](../../plugins/qq-gating.md)。

## 分册

| 册 | 范围 |
|---|---|
| [01a](./01a-core-uidescriptor.md) | Core 侧 UI 描述符链路、状态推送与 epoch 失效 |
| [01b](./01b-core-protocol-lifecycle.md) | 线协议 id 去号码化、旧 QQ 变体生命周期、残留清理 |
| [02a](./02a-panel-protocol.md) | Panel 协议与状态层（通道切换、命令映射、配对语义） |
| [02b](./02b-panel-rewire.md) | Panel 组件数据面重接线 |
| [03a](./03a-panel-renderer-spec.md) | 描述符通用渲染器规范 |
| [03b](./03b-panel-migration.md) | 渲染器迁移与旧组件删除方案 |

## 证据与台账

- [证据 · 冻结接口](./evidence/00-frozen-interfaces.md)：所有分册的共同基线，任何方案不得与之冲突
- 证据包 [01](./evidence/01-evidence-core.md) / [02](./evidence/02-evidence-panel-channel.md) / [03](./evidence/03-evidence-panel-ui.md) / [04](./evidence/04-repo-contention-note.md)
- [05 · 跨文档对账表](./05-reconciliation.md)：悬空引用、冲突点与待裁决项
- [06 · 执行进度台账](./06-execution-progress.md)：各分册的落地状态

## 怎么读这一组

1. 先读 **[00 冻结接口](./evidence/00-frozen-interfaces.md)**——它是判据，不是提议
2. 再看与你要改的部分对应的分册（Core 侧 01、Panel 通道 02、Panel 渲染 03）
3. 有疑问查 **[05 对账表](./05-reconciliation.md)**；想知道「做完了没」查 [06 台账](./06-execution-progress.md)
4. 这类材料是**当时快照**（带基线提交号与行号），与当前代码不一致时以代码为准
