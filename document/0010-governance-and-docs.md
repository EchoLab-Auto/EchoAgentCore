---
id: adr-0010
title: "ADR-0010 治理与文档"
group: 架构决策
x: 2360
y: 888
---
# ADR-0010: 治理与文档(architecture.md + CI 门禁)

状态: accepted

## 问题

重构(Phase 0-5)产出了 12+ 个 crate 与 9+ 条 ADR,但缺少架构脊柱文档
(architecture.md),CI 只有 fmt/clippy/test,依赖方向靠人肉保证;Panel CI
每次克隆 Core master(跨仓库版本漂移)。

## 决策

1. **architecture.md**(doc/architecture.md):镜像 dsh 架构脊柱——设计五原则、
   crate 布局与依赖方向图、能力接缝表、事件模型、turn/step 循环、会话与
   持久化、扩展点地图、关键文档索引。修改包结构前先读它。
2. **依赖方向 lint**(scripts/check-deps.sh):扩展/provider crate 只依赖定义层
   (echo-defs/echo-context/echo-protocol);echo-llm-* 不依赖 agent 框架。
   CI 新增 `deps-lint` job。运行中发现并修复 echo-llm-anthropic 误依赖
   echo-llm-openai(代码未用)。
3. **config 模板测试**(source/core/tests/config_template.rs):`config/
   echo-agent-core.toml` 必须是合法 TOML 且含运行时依赖的 section,防模板
   漂移。并入 cargo test。
4. **Panel CI 钉版**:`ECHO_CORE_REF` 变量钉住 echo-protocol 兼容 commit
   (当前 main,协议变更验证后钉到 commit),消除跨仓库漂移。

## 备选方案

- **不写 architecture.md**:架构靠源码注释,新开发者无入口。
- **依赖方向靠 review**:人肉检查易漏;机器 lint 是硬门禁。
- **Panel 每次克隆 master**:协议变更可随时破坏 Panel 构建,无法复现。

## 后果

- 架构有脊柱文档,修改包结构前先读;ADR 系列(0001-0010)完整记录决策链。
- 依赖方向机器化:违规立即 CI 失败;anthropic 冗余依赖已修复。
- config 模板漂移 CI 拦截。
- Panel 构建可复现(钉版 ref)。
- 全量:Core 470 / Panel 175 测试全绿,双端 clippy 零警告,fmt clean。
