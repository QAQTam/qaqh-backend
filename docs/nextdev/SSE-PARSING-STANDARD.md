# SSE 解析标准（SseDecoder）

> 2026-08-19 定稿。全链路 SSE 解析**唯一标准**，未来重构（Node 重写 / loop 重写 / 前端改造）**必须复用此模式**，禁止回退到旧实现。

## 背景：为什么定这个标准

排查"思考链与正文错排"（BUG：同一 SSE chunk 双字段顺序反转，见 [bugfix-1.1-nextdev.md](./bugfix-1.1-nextdev.md)）与"200 token/s 时表现很慢"时，发现三条 SSE 解析路径各自存在性能/正确性缺陷：

| 路径 | 旧实现 | 问题 |
|---|---|---|
| `qaqh-gate/src/openai.rs`（Chat Completions） | `eventsource-stream` crate | 逐字节 poll 推进，实测 ~180KB/s CPU 吞吐（慢 500×） |
| `qaqh-gate/src/responses.rs`（Responses API） | `Vec::drain(..=line)` 逐行搬移 | **O(n²)**，12MB/20 万行 5 分钟+ 跑不完 |
| `qaqh-client/src/sse.rs` + `timeline.rs`（daemon→前端） | `Vec::split_off`+`extend` 搬移 | **O(n²)** + `String::from_utf8_lossy`（可能注入 U+FFFD 污染中文/emoji） |

## 标准：游标式 SseDecoder

**设计原则（全部满足才叫 SseDecoder）：**

1. **O(n) 总体**：按 `\n` 定位行（`memchr` 风格），用 `consumed` 游标推进，绝不逐帧搬移剩余字节；`push` 时摊销压缩已消费前缀。
2. **严格 UTF-8**：整行 `str::from_utf8` 解码，非法行跳过并告警，**绝不 `from_utf8_lossy`**。
3. **空行定界事件**：SSE 事件由空行结束；多 `data:` 行聚合为单事件（以单个 `\n` 连接）。
4. **兼容字段**：`id:`/`event:`/`data:` 三字段收集；`:` 注释行（keepalive）不产出帧。
5. **零依赖**：仅 `std`，不引任何 SSE crate（`eventsource-stream` 已从依赖移除）。

## 三个变体（同一模式，输出形状不同）

| 文件 | 产出 | 说明 |
|---|---|---|
| `qaqh-gate/src/sse.rs` | `Option<Result<String, ()>>`（data payload） | chat 与 responses 共用，`emit_delta_fields`/`handle_responses_event` 消费 |
| `qaqh-client/src/sse_decoder.rs` | `Option<Result<SseFrame, ()>>`（id/event/data） | daemon→前端事件频道 + timeline 频道共用，`dispatch` 消费 |

## 实测性能基线（单线程 release，10 万–100 万行）

| 路径 | 改造前 | 改造后 |
|---|---|---|
| chat（openai.rs） | ~180 KB/s | **~88 MB/s（~500×）** |
| resp（responses.rs） | O(n²)（超时） | **~147 MB/s** |
| 并发 10 会话流式（qaqh-client 链路同构测试） | — | 并行完成，wall time 与单线程一致，无死锁 |

## 禁止回退清单（未来重构检查点）

- ❌ 禁止重新引入 `eventsource-stream` / 任何 SSE crate
- ❌ 禁止 `Vec::drain(..=line)` / `split_off`+`extend` 逐帧搬移模式
- ❌ 禁止 `String::from_utf8_lossy`（必须严格解码）
- ✅ 新增 SSE 消费路径必须实现/复用 SseDecoder 模式（或移植到 Node 重写时保持同语义：O(n) 游标 + 严格 UTF-8 + 空行定界）

## 相关提交

- `3d945fc`：gate 双字段顺序修复（诊断工具 QAQH_SSE_TRACE 同批）
- SSE 解析标准统一提交（本设计文档同批）：gate `SseDecoder` + qaqh-client `SseDecoder` 双移植