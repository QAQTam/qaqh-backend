# P6 B 路线 1：compact 进 messages.jsonl Handoff（2026-09-25）

状态：**已完成并实机验证**。承接
`docs/plan/2026-09-24-p6-上下文结构解耦设计输入-plan.md` §3 与
`docs/handoff/2026-09-24-p6-segment-partition-recheck-handoff.md` §6。

## 1. 结论

`compact-context.json` 已不再是压缩真源。压缩摘要作为普通 `Message` append 到
`messages.jsonl`，`meta.compact_covered_through_msg_id` 记录被覆盖的最高
`msg_id`；模型活跃视图由归档推导：

```text
leading system messages
+ latest [Compacted N turns] summary
+ every non-summary message with msg_id > compact_covered_through_msg_id
```

摘要使用新的、单调递增的 `msg_id`（不再伪造被压缩 turn 的 id）。
`flat_in_write_order` 单独把摘要渲染在最前，避免新高位 id 把摘要推到尾部。

## 2. 删除的旧链路

- 删除 `CompactContext` 类型及其 `qaqh-session` 导出。
- 删除 `SessionManager::{save,update}_compact_context` 与
  `compact-context.json` 读写。
- 删除 `PersistOp::{UpdateCompactContext,SaveCompactContext}`。
- `MessageStore` 删除 `has_compact_context`、`evicted_prefix` 与
  `evict_compacted_prefix`（旧逻辑压缩驱逐链路）。
- `compact_skip` 字段仅作为旧逻辑压缩回放字段保留；新物理压缩路径恒为 0。

## 3. 崩溃一致性

压缩通过普通 `PersistOp::Append` 落盘：

- 摘要正文与水位字段在同一个 `Append` 中进入 WAL；
- `save_append_with_watermark` 在摘要已归档但 meta 尚未更新时，允许
  watermark-only 重放；
- `replay_message_wal` 对 `fresh.is_empty() && watermark.is_some()` 仍执行该 op，
  避免“摘要已落盘、水位丢失”。
- `SaveFull` 现在携带可选 compact watermark：undo / 图片修复保留标记；
  只有清空全部 turn 时才清除。

## 4. 读侧边界

- `load_for_resume` 返回 `(meta, archive, active)`：第二个元素始终是原始归档，
  第三个元素是模型可见活跃视图。
- `load_archive_tail` 过滤 `[Compacted N turns]`，timeline 继续读真实人类 transcript。
- `load_recent_for_projection` 无 compact marker 时保持有界尾读；有 marker 时
  复用归档推导结果，避免摘要与保留段被尾部窗口截断。
- 旧 `compact-context.json` 文件不再读取；没有 watermark 的旧会话按全量归档恢复
  （符合本轮“不留 v1 兼容层”的裁决）。

## 5. 验证

### 本地门禁

```text
cargo test --workspace -- --test-threads=1              PASS
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
```

### 真机探针

```text
QAQH_SMOKE_LEASE_TTL_MS=30000 ./scripts/v2-smoke.sh /home/qaqtamsy/qaqh-smoke/qaqh
./scripts/v2-content-probe.sh /home/qaqtamsy/.qaqh-content-probe/qaqh
QAQH_CONTENT_PROBE_MODE=permission ./scripts/v2-content-probe.sh /home/qaqtamsy/.qaqh-content-probe-permission/qaqh
./scripts/v2-compact-probe.sh /home/qaqtamsy/.qaqh-compact-probe/qaqh
```

`v2-compact-probe.sh` 的真实链路：

1. 真实 daemon + 本地 OpenAI-compatible fake provider；
2. 发送两轮 >4K token 历史并触发 `conversation_compact`；
3. 断言 `messages.jsonl` 保留旧前缀并追加摘要，meta 水位落盘；
4. 断言不创建 `compact-context.json`；
5. 重启 daemon、resume 后发送第三轮；
6. 断言下一轮 provider 请求含摘要、不含被压缩的旧前缀。

## 6. 未决 / 后续

1. **路线 2 仍未做**：消息正文并入 canonical fact log、`ContextCompacted`
   成为 canonical fact，留给 P6「删除旧目录 / canonical 单源」窗口。
2. **旧 `compact_skip` 字段**仍是 `SessionMeta` wire 字段；新链路不再产生非零值。
   若要彻底清理，需要单独处理 session.list 前端契约与旧 meta 迁移。
3. `snapshot_full` 仍是 undo / 图片修复的整写路径；它现在能保留 compact
   watermark，但图片修复路径的整写成本仍存在，属于后续内存/持久化优化。
