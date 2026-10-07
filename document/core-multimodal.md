---
id: multimodal
title: "多模态输入"
group: 框架
x: 960
y: 640
---

# 多模态输入（图片）

约定与主流多模态 API 一致：**图片只走独立的 image 内容块**（Anthropic `image` block / OpenAI `image_url` part），
文本块里只留占位符。**链路上图片一律是引用、不是内嵌数据**（2026-09-24 媒体库改造）。

## 媒体库：落盘即引用

- 入口即写盘（QQ 图例外）：`~/.local/share/echo-agent-core/media/`（`$ECHO_MEDIA_DIR` 可覆盖），
  **内容哈希命名** = 同图去重 + 天然防穿越 + 强缓存；链路与日志只留 `/media/<id>` 引用
- **QQ 适配器**在门控与过滤管道**通过后**才下载远程图并落盘（单图 ≤10MB 解码字节）——
  被丢弃的消息不白存图（2026-09-29 修正；原为入口即下载、内嵌 data URI）
- **Panel 附图**（最长边 1600px，超 1.5M 字符转 JPEG 逐级缩小）进 `SendMessage` 时由 Core 落盘
- **Panel 展示**：Panel 后端 `GET /media/<id>` 同源提供，浏览器懒加载 + 强缓存

## 模型侧还原

- 发往 LLM 前在**投影出口**（`TrunkStore::reproject_one`）把引用读回 data URI
  （`inline_media_refs_in_messages`）——模型收到的仍是 image 块，与"内嵌时代"无差别；
  落盘文件缺失时丢弃该图并告警，不毒化整段历史
- 落盘失败的图在协议层退化为空串占位（前端渲染「图片已省略」，保留数组长度以便计数）

## 遗留数据迁移

- 加载期一次性把历史事件（含 content 内嵌）与时间线里的 data URI 落盘改写
  （`spill_event_media`，幂等）
- 修正了旧实现"一份图片在 hook JSON / 事件日志 / 时间线各存一份"的膨胀：
  alix 会话 24.6MB → 225KB、时间线快照 8MB → 57KB（面板启动 802ms → 299ms）

## 文本与 token 卫生

- 文本里的 base64 仍要压（防 token 计费）：`echo_defs::media::compact_embedded_media`
  把文本里出现过的附带 data URI 换成 `[图片#n]`（序号与 image 块一致），其余长内联
  data URI 换 `[图片数据已省略]`；`echo-session` 的 `user_message`/`tool_result_message`
  以及两个 provider 各压一次（幂等）。实测 DeepSeek `/anthropic`：4 万 base64 字符
  ≈ 2.8 万输入 token，同一张图走 image 块只要约 200 token（2026-09-10 线上 400 的根因）
- 估算器同源修正（`echo-defs::token`）：base64 连续段按 1 token/字符（原 1/3 少算 >2×）、
  图片按 `编码字节/250` 计（85–8192 封顶）、`tool_calls` 参数计入消息成本；
  `truncate_text_to_tokens` 改为按同一估算器二分切点，避免"截断后仍超预算"
- 单图入参兜底 `MAX_INPUT_IMAGE_CHARS = 16MB`（对齐各入口内嵌上限，只拦异常负载；图片体积不影响 token）

## 相关

- 线协议视角（时间线/事件引用、`/media/<id>`、占位退化）见 [协议与数据流](./protocol.md)§媒体引用
- 会话日志与加载期迁移的落点见 [会话记忆](./core-memory.md)
