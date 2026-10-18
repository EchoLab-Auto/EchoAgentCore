---
id: gen-05
title: "05 · 跨文档对账表"
group: 规划
x: 2240
y: 3480
---

# 跨文档对账表（父级维护）

> 状态：收尾（各册冲突项已裁决；本页保留对账过程）

> 用途：集中管理各设计文档之间的悬空引用、冲突点与待裁决项；全部文档回收后逐项裁决。
> 最后更新：2026-10-16 13:00（01a/01b/02b/03a/03b 已回收；02a 续写中——deepseek）

## A. 悬空引用对账

| # | 提出方 | 引用内容 | 对端 | 对账结果 |
|---|---|---|---|---|
| A1 | 03a §4.1 | 描述符载体：暂按「先读 `AdapterStatus.ui`，再读 `state.ui`」 | 01a | ✅ 01a §1.1 决策：**AdapterStatus 增 `ui`/`session_labels`/`platform`**（选项 a）；03a 的 `state.ui` 回退支可删（01a §3.1 明示） |
| A2 | 03a §4.2 / 03b R2 | 配对键、超时时长、错误文案、断线清理（「02 的约定」） | 02a | ⏳ 02a 续写中（§5.3 将汇总）；已定：键 `adapter-op:${scopedKey(core,adapter)}:${method}` |
| A3 | 03a §2.2 | W2.4 前 `set_owner` 字符串值插件需自行接受 | 01b | ✅ 01a §3.1：text 节点数字/字符串均按字符串显示；01b W2.4 覆盖 id 全链字符串化（含 owner） |
| A4 | 03b §3.1 G4 | Panel `number[]` → `string[]` 与 W2.4 同批 | 01b | ✅ 01b S1-S8 含 protocol.ts:540-548 改造；同批发布（S7 与 Panel 阶段 1 同 tag） |

## B. 交叉决策点终裁

| # | 事项 | 终裁 |
|---|---|---|
| D1 | owner 编辑（v1 无 input 节点） | ✅ **采纳 03a**：`text` 可编辑模式（`editable: true` + action），01a §3.1 已写入 v1 扩展白名单 |
| D2 | 门控提示文案（GATE_HINTS） | ✅ 01a §3.1：select `options` 对象形态 `{value,label?,hint?}` **写入 v1**；字符串数组仍合法 |
| D3 | 名单「从好友/群列表勾选」 | ✅ 01a §3.1：**v1 不覆盖（接受退化）**，v2 加 `suggest`。→ **03b §5 V8/V9 口径修订**：验收断言从「候选来自 get_groups」改为「候选选择不可用（已确认的 v1 退化项）」，其余（列表增删/载荷）不变 |
| D4 | S5/S6 间隔与 core 删除顺序 | ⏳ 待 01b §B 删除计划与 03b 序列合并终裁（01b S7「旧变体加 deprecated + warn，与 Panel 阶段 1 同 tag」；01b S8「N+2 物理删除」。→ 与 03b S6 关系：03b S6 删除 Panel 旧协议引用应在 01b S8 之前、S7 之后 ≥ 一个发布周期） |

## C. 01a 新增的「需修订冻结项」（00 文件需补丁，执行前落实）

1. `AdapterStatus` 追加 `platform? / ui? / session_labels?`（01a §1.1；00 §一字段表）
2. 管理方法表追加 `get_state`（01a §3.1；00 §四）
3. （确认类）v1 扩展字段白名单：select `hint` / image `clear_when` / text `editable` / button `confirm`（01a §3.1；00 §二 UI 描述符）

## D. 02a 关键新事实（F1-F7，由 02a §0 独立核实）

- F1 `InvokeAdapterOp` 失败发 `Error` 而非 `AdapterOpResult{error}` → **Core 前置修复 1**（配对必需）
- F2 读方法不推 AdapterState → **Core 前置修复 2**（+ 新增 `get_state` 方法）
- F4 插件 `parse_ids` 缺字段=清空 → 01a P1 修复口径「部分载荷」与 02a「整对象」互补：P1 修复后 Describe 卡可只带单字段
- F5 插件结果 id 已字符串（groups/friends/filter 四表）→ Panel 新通道下直接全用 string，不必等 W2.4
- F7 `sendPending` 不支持显式 core；`sendCommand` 返回 void → 02a §1-D4 设计（小改）

## E. 02b 对 02a 的接口依赖（02a 需正式回应）

- A1 助手签名与键格式 → 02a §5（已定键格式见 A2 行）
- A2 Core 失败改发 `AdapterOpResult{error}` → 02a §7「Core 前置修复」清单（同上 F1）
- A3 断线时 `readOverlay` 清理钩子 → 02a §5（与 adapterUiByKey 清理同一钩子）

## F. 已回收文档覆盖矩阵

| 文档 | 范围 | 状态 |
|---|---|---|
| 00 | 冻结接口（含本文对账修订） | ✅ |
| 01a | Core 侧 UI 描述符链路 / 状态推送 / epoch / 插件 UI + 切片 S1-S6/P1 | ✅ |
| 01b | W2.4 id 串化 / 旧变体删除计划 / 升级指南草案 / QQ 残留清理 + 切片 S1-S8 | ✅ |
| 02a | Panel 协议与状态层：§0 完成；§1-§7 续写中 | ⏳ |
| 02b | QqPanel/QqLoginSection 数据面重接线 + 切片 R1-R4 | ✅ |
| 03a | Panel 渲染器规范（7 节点 / 接线 / 测试规范）+ parity 核对 | ✅ |
| 03b | Panel 阶段 2 迁移与删除（S1-S7 / 删除清单 / V1-V17 parity / 风险） | ✅ |

## G. 父级抽查勘误（01a）

- 01a §1.1 改造点 1 写「echo-adapter `Adapter` trait（traits.rs:113-156 附近）」——**文件名有误**：
  实际位置为 `echo-defs/src/adapter.rs:111`（`pub trait Adapter`，`:156` 为 `fn management`）。
  行号范围吻合，仅名称旧（echo-adapter 已并入 echo-defs / echo-defs 为该 trait 归属）。
  执行阶段以 `echo-defs/src/adapter.rs:111` 为准。其余抽查声明（adapter.rs:34-62 / :152-270、
  agent/mod.rs:1207-1290、supervisor.rs:869）均属实。
