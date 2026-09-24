# #334 工具 diff 头双斜杠 Handoff（2026-09-24）

状态：**已落地（MR #346 合并，main `84fce95`）。** #334 可关。

## 1. 结论

`unified_diff` 不再无条件给路径拼 `a/` `b/` 前缀：

- **绝对路径** → 原样输出（`--- /tmp/x` `+++ /tmp/x`）；
- **相对路径** → 保持 git 惯例（`--- a/src/x.rs` `+++ b/src/x.rs`）。

issue 的三选一里选**选项 1**。选项 2（前缀 + 相对工作区路径）看似更贴 git 惯例，但
要把 workspace root 贯穿 `unified_diff` 的每个调用点（`write` / `edit` / `copy_range`
现场 + journal 落盘的 `file` 字段与事后 `export_patches` 重放），是契约级改动，超出
这条 P3 观感 bug 的收益；选项 3（只去重复分隔符）会让绝对路径看起来像相对路径，明确
不选。

## 2. 改动位置

| 文件 | 内容 |
|---|---|
| `crates/qaqh-workspace/src/file_shared.rs` | 新增 `diff_header_labels` / `is_absolute_path`；`unified_diff` 改用它 |
| `crates/qaqh-workspace/src/file_mutate.rs` | dry_run 断言改为「含 `--- <绝对路径>` 且不含 `a//`/`b//`」；model 面不泄漏 diff 的断言从 `--- a/` 收紧到 `--- ` |

`is_absolute_path` 是**展示面专用**的跨平台判断（Linux 绝对路径 / Windows 盘符 /
UNC），不参与任何路径解析或安全校验。

调用点覆盖面：`write`（含 dry_run 预览）、`edit`、`copy_range`、`journal export` 的
patch blob。`display.diff` 是纯展示面（gate 与模型投影都不消费），**无 wire 变更、
无 canonical fact 变更、未动 `payload_version`**。

## 3. 验证证据

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                              PASS
scripts/v2-smoke.sh <data-root>                         PASS（全阶段，含 restart reclaim）
```

实机（真实 daemon + 真实 PTY/TUI 回合）：用本仓 daemon 跑
`qaqh-tui-app/scripts/e2e-alpha1-basic.sh`（fake OpenAI-compatible provider 按脚本下发
`write` → `read` → `edit` → `exec`，TUI→daemon→provider→工具→回灌链路全真），从
`/tmp/qaqh-e2e-alpha1-basic/tui.raw` 原始字节流里取出两条 diff：

```text
--- /tmp/qaqh-e2e-alpha1-basic/work/alpha1-basic.txt +++ … @@ -0,0 +1 @@ +alpha1-basic-v1
--- /tmp/qaqh-e2e-alpha1-basic/work/alpha1-basic.txt +++ … @@ -1 +1 @@ -alpha1-basic-v1 +alpha1-basic-v2
```

整条原始流 `a//` / `b//` 零命中（修复前正是这两条 header 带双斜杠）。

**冒烟脚本已知抖动**：宿主机有并行 release 构建时 load 会飙高，6s lease TTL 会让
`driver release` 段假红（表现为 bootstrap 返回里没有 `control` 字段，harness 抛
`KeyError: 'control'`）。用 `QAQH_SMOKE_LEASE_TTL_MS=30000 ./scripts/v2-smoke.sh <root>`
复跑即绿，与本改动无关。

## 4. 接手注意

- 若后续有人要做「相对工作区路径」的 diff 头（issue 选项 2），必须先解决
  `journal::record_change` 存的是**调用方原样路径**这件事：事后 `export_patches` 手里
  没有 workspace root，改前缀就要连带改落盘字段（含历史 journal 兼容）。
- 相对路径的 header 逐字未变，`journal export` 的 `--- a/a.txt` 断言原样通过；
  绝对路径 header 变成裸路径后，`patch -p0` 语义反而更直白。
- 别把 `is_absolute_path` 当路径安全判断用：它是给 diff 头看的，不 resolve、不
  canonicalize，也不处理 `..`。

## 5. alpha 未决清单（截至本次）

1. **#345**（P1）pending interaction 的 request 正文不在 canonical log —— v2 重连无法
   重建 modal。三条路线（A 进 canonical fact / **B 内容落盘 + `/ringing/v2/content/{id}`** /
   C bootstrap 内联）仍待裁决；B 卡在 content 端点语义未定（TTL / quota / 授权 / 上传）。
2. **#336**（P2）工具展示数据源重构：summary 不该是「输出首行」。
3. **#339**（P3，挂起）P6 设计输入：上下文结构解耦。
4. **P6 B**：compact 去掉第二真源（`compact-context.json` → marker 进 `messages.jsonl`）。
5. **v1 `Last-Event-ID` → v2 cursor 映射**（已随 v1 端点硬切作废，见
   `2026-09-24-tool-outcome-p2-v1-cursor-closure-handoff.md` §1）。
6. **V2-C3 replaceable producer**（已补，见
   `2026-09-24-ringing-v2-alpha-closure-handoff.md` §2.7）。
7. driver 侧：3s 巡检/重启回收与 workspace service gate 已补；仅剩
   `not_eligible` / 显式移交优先级。
8. 崩溃路径 fence 轮转（`ToolLedger::Drop` 只覆盖有序退出）。
9. 两套 interaction kind 拼写不一致（bootstrap `plan_review` vs SSE delta `plan`），
   见 `2026-09-24-323-typed-payload-gap1-handoff.md` §2。

## 6. 相关文件

| 内容 | 路径 |
|---|---|
| diff 头生成 | `crates/qaqh-workspace/src/file_shared.rs` |
| write 展示面 | `crates/qaqh-workspace/src/file_mutate.rs` |
| edit 展示面 | `crates/qaqh-workspace/src/edit/handler.rs` |
| copy_range 展示面 | `crates/qaqh-workspace/src/copy_range.rs` |
| journal patch blob / export | `crates/qaqh-workspace/src/journal.rs` |
| 实机 e2e（TUI 仓） | `qaqh-tui-app/scripts/e2e-alpha1-basic.sh` |
