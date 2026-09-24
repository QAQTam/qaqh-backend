# 工具 stdout/stderr 分离 P3 Handoff（2026-09-24）

状态：**已落地，本地全量门禁 + 真实 daemon exec probe 全绿。**

## 1. 目标

把 exec 的展示面从单一合流文本升级为结构化 stdout/stderr，同时保留旧
`Shell.output` 和 `TimelineTool.output` 作为兼容回退。

## 2. Wire / SDK 形态

新增：

```rust
ToolBody::Streams {
    stdout: String,
    stderr: String,
    exit_code: Option<i32>,
    truncated: bool,
    interleaved: bool,
}
```

对应：

- `qaqh_types::ToolResultDisplayBody::Streams`
- `qaqh_domain::TimelineToolBody::Streams`

规则：

- `interleaved = false`：两流已分开，不承诺真实交织顺序；
- `exit_code` 仍在 body 内，旧 client 不必从 summary 猜；
- 旧 `Shell` / `output` / `summary` / `outcome` 全部保留；
- stdout/stderr 是 display-only，`ExecOutput` 用 `#[serde(skip)]` 承载，
  不进入模型 JSON；
- stdout/stderr 各自经过 ANSI 清理、CR 归一化和 display body 上限截断；
- `truncated` 是两条流截断与模型输出截断的并集。

## 3. 生产链路

```text
direct.rs 捕获 stdout_out / stderr_out
  -> ExecOutput { stdout, stderr }  // display-only
  -> ExecOutput::display()
  -> ToolBody::Streams
  -> ToolOutcome::to_tool_result()
  -> ToolResultDisplayBody::Streams
  -> runtime wire_display()
  -> TimelineToolBody::Streams
  -> timeline snapshot / SSE / client
```

只有 stdout/stderr 都缺失的 legacy JSON 才回退 `ToolBody::Shell { output }`。

## 4. 验证

```text
cargo test --workspace -- --test-threads=1              PASS
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
QAQH_SMOKE_LEASE_TTL_MS=30000 scripts/v2-smoke.sh        PASS
scripts/v2-content-probe.sh                              PASS（ask）
QAQH_CONTENT_PROBE_MODE=permission scripts/v2-content-probe.sh ...
                                                         PASS（permission）
```

permission probe 真机新增断言 ⑦：

```json
{
  "kind": "streams",
  "stdout": "permission-probe\n",
  "stderr": "",
  "exit_code": 0,
  "truncated": false,
  "interleaved": false
}
```

同时保留断言 ⑥ 的结构化 `outcome`。

## 5. 仍未决

- interaction 正文跨 daemon 重启持久化；
- permission 正文 pinned + 终结 unpin；
- driver `not_eligible` / 显式移交优先级；
- 崩溃路径 writer fence 轮转；
- #339 B 路线 1（独立窗口）。

## 6. 接手注意

- 不要把 stdout/stderr 合并回单一 `Streams` 字段；需要合流时读旧 `output`。
- 不得声称 `interleaved=true` 除非 pipe 层有全局序号；当前始终 `false`。
- 旧 client 遇到未知 `streams` 变体必须完整回退旧字段，不得只渲染半个卡片。
