# windows-reactor 上游适配记录（2026-08-12）

> 本文档面向后续接手的 LLM / 开发者：**开始改动 `apps/winui` 或 `deepx-winui-reactor` 之前必读**。
> 记录 2026-08-12 合并上游 microsoft/windows-rs 5 个 commit 后造成的破坏性变更、已完成适配与遗留事项。

---

## 1. 依赖关系

- `F:\DeepX\apps\winui\Cargo.toml` 通过 **path 依赖**引用 fork 仓库：
  ```toml
  windows-reactor = { path = "../../../deepx-winui-reactor/crates/libs/reactor" }
  windows-reactor-setup = { path = "../../../deepx-winui-reactor/crates/libs/reactor-setup" }
  ```
- `deepx-winui-reactor` 是 `microsoft/windows-rs` 的 fork，分支 `deepx-winui` 为开发主线。
- 上游同步流程：`git fetch upstream && git merge upstream/master`（已有先例，merge 无冲突策略可行）。

---

## 2. 本次上游变更（#4825–#4829，2026-08-11 合入）

| Commit | 主题 | 性质 |
|---|---|---|
| `850343478` #4825 | 失败恢复 teardown 可靠性 | 内部修复，无 API 变更 |
| `1112d6e6d` #4826 | effects 改在 native 提交（drain realizations）之后执行 | 内部时序调整 |
| `6f924b79f` #4827 | seal 内部 API（`pub`→`pub(super)`）、删除死代码（`Reconciler::update`） | 内部收敛 |
| `85ca07a47` #4828 | **删除 error boundaries** | ⚠️ 破坏性 API 删除 |
| `5f8e1504d` #4829 | **callback panic 改为 fatal（abort）** | ⚠️ 破坏性 API 删除 + 语义变更 |

### 破坏性变更明细

**#4828 删除：**
- `error_boundary()`、`Fallback`、`ErrorBoundaryElement`
- `Element::ErrorBoundary` 变体
- `panic_message()` 辅助函数
- 连带删除上游的 error_boundary 示例/测试/文档

**#4829 删除：**
- `App::on_fault()` 方法
- `Fault` 类型、`fault::catch` / `fault::set_handler`
- fault.rs 从 151 行精简为 28 行，仅剩 `abort_on_panic` / `report`

### 语义变更（最重要的行为差异）

| 场景 | 之前 | 现在 |
|---|---|---|
| callback panic（渲染/事件/timer） | log-and-continue（on_fault 可接住） | **reactor emit 一行诊断日志后 `std::process::abort()`** |
| turn 渲染失败 | error_boundary 降级显示 fallback，应用继续 | 传播到 render 边界 → **整个进程 abort** |

abort 前日志格式：`windows_reactor: {context} panicked: {msg}; aborting`（OutputDebugString / stderr 可抓）。

---

## 3. 已完成适配

### 3.1 fork 仓库（deepx-winui-reactor，已 push `06c114742`）

- merge commit `27541a0b1`（上游 5 commit 合入）
- `06c114742`：修复 3 个过时 doctest —— **DeepX 本地扩展**（见 §4.1）让上游 `compile_fail` 示例意外编译通过，已改为普通示例并注明。
- 验证：`cargo check -p windows-reactor --all-features` ✅、`cargo test -p windows-reactor` ✅（lib 17 + doctest 16）

### 3.2 下游（F:\DeepX，已提交 `f57ea53`，未 push）

| 文件 | 改动 |
|---|---|
| `apps/winui/src/main.rs` | 删除 `.on_fault(...)` 调用（含原注释，替换为新语义说明） |
| `apps/winui/src/chat_view.rs` | `turn_memo` 去掉 `error_boundary` 包装直接渲染 `turn_body`；删除 `turn_render_fallback` 死代码 |
| `apps/winui/src/diagnostics.rs` | `record_fault` / `Event::Fault` 加 `#[allow(dead_code)]` **保留**（诊断链无生产者） |

验证：`cargo check -p deepx-winui` ✅ 0 警告、`cargo test -p deepx-winui` ✅ 80 passed。

---

## 4. DeepX 本地扩展（fork 特有代码，merge 上游时重点关照）

### 4.1 `crates/libs/reactor/src/element.rs` — `impl Element` 便捷方法（约 105 行，标记 "DeepX downstream extension"）

`Element` 上直接提供：`modifiers_mut`、`margin`、`horizontal_alignment`、`vertical_alignment`、
`grid_row`、`grid_column`、`transition`、`automation_name`、`automation_id`、
`keyboard_accelerator`、`on_pointer_pressed`；另有 `TextStyleExt::foreground_gradient`。

**注意**：上游的 Ext trait（`AccessibilityExt`/`GridChildExt`/`InputExt` 等）只对具体 widget 实现，
它们的 `compile_fail` doctest 演示「在 `Element` 上调用会编译失败」——DeepX 扩展后这些示例会意外
编译成功。**每次 merge 上游后跑 `cargo test -p windows-reactor --doc` 检查**，若有类似失败，
按 §3.1 方式改为普通示例（保留 DeepX 语义说明）。

### 4.2 其他 fork 特有改动（与上游零重叠，merge 一般无冲突）

- `crates/libs/reactor/src/backend/mod.rs`、`backend/winui/generated_set_prop.rs`
- `crates/libs/reactor/src/bindings.rs`、`generated.rs`
- `crates/libs/reactor/src/widgets/scroll_viewer.rs`

---

## 5. 遗留事项 / 后续提醒

1. **diagnostics fault 链断了**：`record_fault`/`Event::Fault` 无生产者（`allow(dead_code)` 挂着）。
   若上游未来提供新的 fault hook（如 `abort` 前回调），优先重新接回，恢复闪退调查的 fault 事件。
2. **flash-crash 调查**：现在 panic 即 abort，源头证据只有 reactor abort 前的
   `windows_reactor: {context} panicked: ...` 一行；如需更多信息，考虑挂 `OutputDebugString` 捕获。
3. **不要再使用** `error_boundary` / `on_fault` / `Fault` / `ErrorBoundaryElement` —— 上游已删除，
   使用会导致编译失败。
4. **上游同步节奏**：上游 windows-rs 迭代快（近期几乎每天有 reactor commit），建议定期
   `git fetch upstream && git merge upstream/master`，merge 后必跑：
   ```bash
   cargo check -p windows-reactor --all-features
   cargo test -p windows-reactor        # 含 doctest，检查 §4.1 的 compile_fail 冲突
   cargo check -p deepx-winui           # 下游编译
   cargo test -p deepx-winui            # 下游测试
   ```
5. **下游其他未提交工作**（截至 2026-08-12，与本适配无关，提交时注意区分）：
   `apps/winui/src/bridge.rs`、`settings_view.rs`、`crates/deepx-app-notifications/src/lib.rs`、
   `apps/winui/Cargo.toml`、`Cargo.lock`。
