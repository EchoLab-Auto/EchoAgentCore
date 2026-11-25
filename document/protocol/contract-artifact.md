---
prodoc: true
id: protocol-contract-artifact
title: "管理面契约归档"
group: 工程
link: [protocol]
x: 9832
y: 48
---

# 管理面契约归档

`echo-protocol` 持有管理面线格式。契约归档把它生成的 TypeScript、JSON 消息样例和来源清单打在一起；解包核验不需要 Core 源码、Rust 工具链或 Panel 的 `node_modules`。归档不改变运行时 WebSocket 协议，也不扩大 Core 与 Panel 的混部兼容承诺。

## 发布流程

Core 的 `release.yml` 先从 `Cargo.lock` 找到配对的 Panel 提交，比较该提交的 `protocol.generated.ts` 与 Core 生成的绑定。缺文件或字节不一致即停止。归档任务随后从本次 Core 提交快照读取生成器、版本和默认夹具，核对绑定摘要后打包。推送版本标签时，归档和校验和随原生包一起发布；手动运行只生成工作流产物，不发布。

| 文件 | 用途 |
| --- | --- |
| `protocol.generated.ts` | 由 `echo-protocol` 生成的绑定，不手改 |
| `fixtures/*.json` | 脱敏的消息样例，覆盖时间线同步、翻页和模块调用 |
| `fixtures.index.json` | 样例文件、摘要与消息形状索引 |
| `manifest.json` | 契约版本、Core 来源、工具链及逐文件摘要 |
| `SHA256SUMS` | 解包后快速核验清单与文件内容 |

契约版本默认随 Core 工作区版本。Rust 消费方可用 `echo-protocol` 的 Git 依赖并固定完整提交；当前不发布 crates.io 包。Panel 的 `protocolCore.commit` 是生成或核对绑定的源码基线，不一定是发布归档的 Core 提交；两个不同提交生成相同的绑定是正常情况。

## 无 Core 源码核验

先用发布附件的 `.sha256` 校验压缩包。解包后，用可信的 `verify-protocol-contract.py` 核验目录；`CORE_RELEASE_SHA` 取该发布对应的完整 Core 提交，`EXPECTED_TS_SHA256` 取配对 Panel 所记录的绑定摘要，而不是直接信任待校验归档自己的清单。

```bash
python3 verify-protocol-contract.py "$CONTRACT_DIR" \
  --expect-commit "$CORE_RELEASE_SHA" \
  --expect-ts-sha256 "$EXPECTED_TS_SHA256" \
  --require-fixtures 8
```

文件缺失、夹带未登记文件或摘要不符都会失败。来源核验会拒绝标记为 `dirty=true` 的本地产物。Panel CI 的源码严格校验与此互补：前者证明源码能生成绑定，后者证明收到的发布附件与预期来源及绑定相符。

## 本地维护

只运行生成器时，不需要构建 Core 服务：

```bash
cargo run --locked -p echo-protocol --features ts-bindings \
  --example protocol_contract_ts > /tmp/protocol.generated.ts
```

用 `scripts/build-protocol-contract.py --help` 查看装配参数。在未提交工作树测试时显式传 `--allow-dirty`；外部夹具或预生成 TS 同样只允许用于本地测试，并会标志产物不可作为固定提交的发布附件。运行 `bash scripts/tests/protocol-contract-artifact.test.sh` 可检查归档、摘要、缺失文件以及历史提交的溯源。主动更新线格式时，给该测试脚本传 `--expected <sha256>` 提供新的配对绑定摘要；装配脚本对应的参数名是 `--expect-ts-sha256`，别混用。
