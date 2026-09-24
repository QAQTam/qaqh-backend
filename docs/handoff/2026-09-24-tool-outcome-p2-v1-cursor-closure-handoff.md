# 工具终态 P2 + V2-V1 作废 Handoff（2026-09-24）

状态：**已落地，本地全量门禁 + 真实 daemon probe 全绿。**

## 1. 本轮裁决

1. **V2-V1 作废**：v1 端点已整体硬切，v1 emitter / mapping sidecar 没有生产
   链；继续实现 `Last-Event-ID → v2 cursor` 只会成为伪造兼容层，违反
   「不留 v1」裁决。
2. **#336 P2 落地**：工具展示面增加结构化 `outcome`，终态不再从
   `summary` / `[OK]` 文本反推。

## 2. #336 P2 改动

### 2.1 Wire 类型

新增：

```rust
ToolResultDisplayOutcome {
    state: Succeeded | Failed | Cancelled | TimedOut | Backgrounded | Unknown,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    output_bytes: Option<u64>,
    truncated: Option<bool>,
}
```

`ToolResultDisplay` 与 `TimelineToolDisplay` 增加 optional `outcome`；旧字段
`summary` / `diff` / `header` / `body` / `metrics` 全部保留。

### 2.2 生产路径

- SDK 内部新增 `ToolDisplayOutcome` / `ToolTerminalState`；
- `ToolOutcome::to_tool_result()` 作为框架填充点：
  - `state` 从 `ToolStatus` + `ToolErrorKind` 派生；
  - exec 的 `timed_out` / `cancelled` 由 `ExecOutput` 显式声明；
  - `exit_code` / `truncated` 从 typed body 派生；
  - `duration_ms` / `output_bytes` 从框架 metrics 填充；
- `map_tool_result` 保留 wire display 的 `outcome`，legacy 往返不丢字段；
- `wire_display` 是 SDK → timeline 的唯一映射点；
- timeline 重建从 `messages.jsonl` 的 canonical display 恢复 `outcome`，快照
  与 live 同形。

## 3. 验证

```text
cargo test --workspace -- --test-threads=1              PASS
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
QAQH_SMOKE_LEASE_TTL_MS=30000 scripts/v2-smoke.sh ...    PASS
scripts/v2-content-probe.sh ...                          PASS（ask）
QAQH_CONTENT_PROBE_MODE=permission scripts/v2-content-probe.sh ...
                                                         PASS（permission）
```

permission probe 新增真机断言 ⑥：真实 daemon 执行 `echo permission-probe`
后，从 v2 timeline 快照读回：

```json
{"state":"succeeded","exit_code":0,"duration_ms":53,
 "output_bytes":156,"truncated":false}
```

这证明终态来自结构化 outcome，而不是 summary 文本。

## 4. 仍未决

- interaction 正文跨 daemon 重启持久化；
- permission 正文 pinned + 终结 unpin；
- driver `not_eligible` / 显式移交优先级；
- 崩溃路径 writer fence 轮转；
- #336 P3：stdout / stderr 分离（v2 延后）；
- #339 B 路线 1（独立窗口）。

## 5. 接手注意

- `outcome` 是框架投影，不是工具作者自由填写的第二个事实源；
- 没有 metrics 的 legacy 结果不得用 `0` 覆盖已有 duration/output_bytes；
- 未知 wire state 反序列化为 `Unknown`，不得降级成 `Failed`；
- v1 cursor 映射不要恢复；若未来重新引入 v1 兼容面，必须先恢复 v1
  emitter + mapping sidecar，并重新评审 V2-V1。
