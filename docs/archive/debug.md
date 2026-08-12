# ASK 弹窗链路排查——交接总结

> 生成时间：2026-08-13（RC4 收尾）
> 状态：断点已锁定在 winui 内置 overlay 状态投影段，待修复方接手

---

## 一、最终结论（一句话版）

**传输链路全通、引擎行为全对、外置渲染正常——只有 winui 内置 `interaction_overlay` 不显示（且 composer 不 gate），断点在「`InteractionMachine.apply` → `refresh_interaction_snapshot` → main.rs UI 消费」的状态投影段。**

---

## 二、证据链（按验证顺序）

| # | 验证项 | 结果 | 证据 |
|---|---|---|---|
| 1 | 工具参数 | ✅ 完好 | `C:\Users\QAQTam\.deepx\sessions\53769cb9\messages.jsonl` 原始 JSONL 完全正常（`@{...}` 是 PowerShell 转换显示假象，已排除） |
| 2 | 引擎拦截/挂起 | ✅ 正常 | `agent.log`：t21 admit ask 后 40 秒零进展 + `auto-complete: 1 unfulfilled tool(s) — [CANCELLED]` |
| 3 | 引擎 emit | ✅ 正常 | 外置实验证明 `InteractionRequested` 从引擎发出并到达前端 |
| 4 | worker→daemon IPC | ✅ 正常 | envelope 序列化、stdout 管道、daemon reader 解析全通 |
| 5 | hub→SSE→client | ✅ 正常 | control 频道事件正常送达 bridge |
| 6 | bridge 事件分发 | ✅ 正常 | `apply_interaction_event` 被调用（demo 被拉起） |
| 7 | 外置 UI 渲染+交互 | ✅ 正常 | `interaction-demo.exe` 弹窗出现，用户点击提交，答案写入 `%TEMP%\deepx-ext-ask-call_00_EIMcxWJJTzOWCjyFO7sz4407.json`（`resolution: answered`，答案："看到了独立弹窗"） |
| 8 | **内置 overlay 显示 + composer gate** | ❌ **失败** | 多次实验均无弹窗、无 gate——与传输无关，纯前端投影/渲染问题 |

**关键实验**：winui bridge 在收到 `InteractionRequested` 时拉起独立 `interaction-demo.exe`（CLI 模式），用户点击后答案回写 temp 文件——完整闭环跑通，证明「引擎 → 前端事件」链路 100% 通。

---

## 三、断点精确位置（待修）

```
apps/winui/src/bridge.rs:
  L3140  interaction_event(event) → apply_interaction_event(&batch.seed, ev)   ← 事件已到 ✅
  L1284  apply_interaction_event → interactions[seed].apply(ev)                ← 机器更新
  L1301  refresh_interaction_snapshot → 只投影 active_seed() 的机器            ← 头号嫌疑
  L1246  interaction_snapshot() → (InteractionState, rev)
apps/winui/src/main.rs: 读 interaction_snapshot + interaction_rev → interaction_overlay 渲染  ← 次嫌疑
```

### 头号嫌疑
`refresh_interaction_snapshot`（bridge.rs L1301）用 `active_seed()` 投影——若 active seed 与事件 seed 不一致（未设置 / 格式不同 / 前缀差异），快照恒空 → 不弹窗 + 不 gate，与现象完全吻合（**两个症状同源**）。

### 次嫌疑
1. main.rs 消费侧未订阅 `interaction_rev` 变化（rev 比对失败 → UI 不刷新）
2. `InteractionMachine.apply(AskRequested)` 未正确设置 `active_ask_plan`（`has_pending` 恒 false → composer gate 失效）

### 验证顺序建议
1. 在 `refresh_interaction_snapshot` 打日志：确认 machines 更新后 snapshot 是否非空、`active_seed()` 与 `batch.seed` 是否一致
2. 查 main.rs 的 `interaction_rev` 消费与 overlay 渲染条件
3. 复现：调 ask 工具 → 看 demo 弹窗出现（证明事件到）→ 看内置 overlay 是否显示

---

## 四、工作区现状（未提交改动）

### 诊断用（闭环验证后待清理/决策）
| 改动 | 位置 | 说明 |
|---|---|---|
| ASK-V2 旁路 | `crates/deepx-ringing/src/worker.rs` + `lib.rs`、`crates/deepx-msglp/src/ringing_v1/`（types/paced_emitter/wire/loop_core/engine_turn）、`crates/deepx-runtime/src/registry.rs` | 独立 wire 极简 ask 帧（`RingingAskEnvelope` + `WIRE_RINGING_ASK_V1`），engine_turn L2055 双发 + registry L289 旁路解析。**已证实传输通，此旁路可回滚或保留为加固** |
| 外置弹窗探针 | `apps/winui/src/bridge.rs` `spawn_external_ask` | 事件到达即拉起 `F:\test-winui-rs\target\debug\interaction-demo.exe`（多候选路径：`DEEPX_EXT_ASK_DEMO` 环境变量 → dev/release target → exe 同目录） |
| demo CLI 模式 | `F:\test-winui-rs\crates\interaction-demo` | `--ask <json> --out <path> --id <id>`，启动即弹 ask 模态，提交/跳过写答案文件并退出 |

### 已完成的正式改动（未提交，但可保留）
- diff 面板 overlay（`apps/winui/src/diff_drawer.rs` V7：70% 尺寸、scrim、Esc/✕ 关闭）+ z-order 修复（main.rs）
- reactor binding `closing` 加固（`F:\deepx-winui-reactor\crates\libs\reactor\src\backend\winui\mod.rs`，135 fixtures 过）
- header 调试按钮（`spawn_test_ask`，Symbol::Send）——修复后建议移除

### 构建状态
- `cargo check -p deepx-winui` / `-p deepx-msglp` / `-p deepx-runtime` / `-p deepx-ringing` 全部 ✅
- msglp 测试 40 + 29 ✅；两仓库 `diff --check` 干净

---

## 五、RC4 收尾清单（公测前）

1. **[关键路径] 修内置 overlay 投影**（第三节断点，预计 1-2h）
2. 端到端复验：弹窗 + gate + **答案回填引擎**（AskResponse 命令闭环——外置 demo 已证明 UI 侧可收集答案，但引擎回填未实现，当前 ask 会被下一条消息 auto-complete 取消）
3. 清理诊断代码：ASK-V2 旁路去留、`spawn_external_ask` 探针、header 调试按钮、demo CLI 模式
4. diff panel 实机验证（70% 尺寸、✕/Esc 关闭、快速重开）
5. 统一 winui 日志路径（用户跑安装版，`DEEPX_WINUI_LOG` 未指向 dev 日志 `apps/winui/.deepx-winui.log`，当前实例日志位置未知）

---

## 六、复现路径（环境已就绪）

1. daemon 已含 ASK-V2 编译（agent.log 有 `[ASK-V2]` 日志点）
2. winui 已含外置弹窗探针（bridge `spawn_external_ask`）
3. demo exe 在 `F:\test-winui-rs\target\debug\interaction-demo.exe`
4. 复现：让 agent 调 ask 工具 → 观察：①demo 弹窗（事件到达证明）②内置 overlay ③composer gate ④`agent.log` 的 `[ASK-V2]` 三日志点（emit / daemon received / published）

---

## 七、关键代码位置速查

| 文件 | 位置 | 内容 |
|---|---|---|
| `crates/deepx-msglp/src/ringing_v1/engine_turn.rs` | L2005-2060 | suspend 条件 + `emit_active_ask`（L2055）+ ASK-V2 双发 |
| `crates/deepx-msglp/src/ringing_v1/engine_turn.rs` | L522-543 | `emit_active_ask`（构造 InteractionRequested） |
| `crates/deepx-msglp/src/ringing_v1/engine_tool.rs` | L422 附近 | `auth.tool_name() == "ask"` 拦截分支 |
| `crates/deepx-workspace/src/ask_user.rs` | L32-130 | `normalize_ask_user` 参数校验 |
| `crates/deepx-runtime/src/registry.rs` | L277-330 | daemon worker reader（ASK-V2 旁路解析在 timeline 分支前） |
| `crates/deepx-runtime/src/ringing/hub.rs` | L994-1100 | `publish_with_causation`（journal + fanout） |
| `apps/winui/src/bridge.rs` | L1284 / L1301 / L1246 | `apply_interaction_event` / `refresh_interaction_snapshot` / `interaction_snapshot` |
| `apps/winui/src/bridge.rs` | L461-521 | `interaction_event` 解析（ControlEvent → InteractionEvent） |
| `apps/winui/src/bridge.rs` | L523-580 | `spawn_external_ask`（外置弹窗探针） |
