# Handoff — 旧协议清理 P0 死面 / P1.1（2026-10-05）

> 任务来源：`docs/audit-legacy-protocol-2026-10-04.md`（§7 移除顺序，**权威**）
> + `docs/plan-legacy-protocol-cleanup.md`（互补计划；audit §6 列了它已失效的条目）。
> 当前状态：**P0 死面 + P1.1（MCP/LSP typed 化 + v1 注册面 + per-call TLS 视图）全部完成，
> 已 fast-forward 进 `main`**。`main@5b1b7fa`。P0 尾巴（④ ts-rs 白名单）、P1.2、P1.3、P2、P3 未做。
> 本文件只给接手的人：**已落地什么、已验证到什么程度、哪些结论不必重查、剩下的挂点在哪**。

## 一、已落地（commit 链，均为 main 上真实提交）

| commit | 内容 |
|---|---|
| `9701ca0` | 批次 1a 死面：领域 `Delivery`、ringing 握手 `ClientOpenRequest/Response`、`RingingCommandEnvelope`、qaqh-session `canonical_identity_for_session`/`rename_session`、前端 `bootstrap()` 占位、ts 孤儿 |
| `b515d3b` | 批次 1b①：v1 渠道态孤儿（`SessionChannelState`/`ChannelShards`/`sequencer`/`SnapshotProjector`/`conversation_snapshot`）+ `RINGING_VERSION`；含删 `crates/qaqh-ringing/src/snapshot.rs` |
| `59b65bd` | `wal::read_ops` 不失败旧签名 |
| `d0012f3` | v1 `ToolManager::register` 移出生产面（`test-harness` 门控） |
| `c76e516` | **P1.1a**：删 `RegisteredTool.legacy` / `LegacyExecutor` / `PreparedExecutor`（两臂合一）；`execution.rs` 单一 typed 执行臂；`PreparedCall` 增 `effective_timeout` |
| `ee26518` | **P1.1b**：`DynamicTool.handler_fn`（v1 `fn(ToolCallCtx)->ToolResult`）→ `DynamicDispatch`（`fn(&str,&ToolCallContext,Value)->Result<ToolOutcome,FatalToolError>`）+ 新增 `tool_api/dynamic.rs`；MCP/LSP dispatcher 换契约，LSP root 改读 `ctx.workspace_root` |
| `d08db70` | **P1.1-②**：退役 `{name}_{action}` 复合名（删 runtime `resolve_effective_name`、workspace `resolve_name`、`ToolInvocation.action`、`AuthorizedToolCall::action()`、`PermissionChallenge.action`、wire `ToolCommand::ToolInvoke.action`） |
| `5b1b7fa` | **P1.1-①**：删 `ToolHandler`/`LegacyToolAdapter`/`ToolCallCtx`/`install_tool_call_context`/`ToolCallContextGuard`/`TOOL_CALL_CANCEL`；`tool_api/legacy.rs`(799 行) → `tool_api/result.rs`(311 行)；新增 `probe.rs`（test-harness 的 typed 探针） |

> 另有 `1d6d662`（157 文件注释审计合并提交）与同事的 `ef796f7`（daemon 设备鉴权 spec），
> 非本清理线。

## 二、已确证的事实（**别重复调查**）

1. **v1 注册面生产从未使用**：`registration::build_tool_manager` 下 20 个内置工具全是 `register_typed`；
   `register()` 早在 `d0012f3` 就 `#[cfg(any(test, feature="test-harness"))]` 门控。删它零生产行为变更。
2. **MCP/LSP 内部仍产 v1 `ToolResult` 信封**（模型面 §7 逐字节契约不能动），只在边界经
   `map_tool_result`（`crates/qaqh-workspace/src/tool_api/result.rs`）收口到 typed 结果面。
   → **`map_tool_result` 是承重点**，删 `legacy.rs` 时我补回了 10 例映射契约单测（原模块 19 例测试随文件被删）。
3. **`{name}_{action}` 机制生产已死**：`ToolCommand::ToolInvoke` 全仓只有 1 单测 + 1 集成测试两个
   构造点，无真实客户端 producer（webui 只有生成类型、无调用点）；模型路径经
   `authorize_call_with_context` 本就恒传 `action: ""`。wire 删字段向后兼容（serde 默认忽略未知字段）。
4. **取消语义（P1.1-① 的判据，曾误判过）**：`file_mutate::is_cancel()` 与
   `file_mutate::ambient_tool_context` **全是 test-only compat 入口**（`exec_read`/`exec_edit`/
   `exec_apply_patch`/`exec_journal`/… 的调用者都在 `#[cfg(test)]` 模块内），生产面不走。
   生产面 `exec/direct.rs` 与 `process_inspect.rs:139` **本来就传 `ctx.cancellation` 显式 token**，
   `|| crate::is_cancel()` 只是兜底。per-call 取消的实际通道是
   `ToolCallContext.cancellation` 的共享 `Arc` + `ToolManager.inflight_tasks`（`cancel_tool(Some(id))` 写同一 Arc）。
   → 删 per-call TLS 视图**不**影响取消；取消三件套已复验全绿。
5. **审计账本不能删字段**：`AuditEntry.action` / `v2::ToolInfo.action` 参与链哈希
   （`v2.rs::record_hash` 会**重序列化整条记录**再比对）。删字段会让**既有账本校验失败**。
   ② 因此保留该字段、值恒空并加注（`crates/qaqh-workspace/src/audit/mod.rs`）。要真正删需单独做账本 schema 迁移。
6. **工程坑**：新 `git worktree` 缺 gitignored 的 `webui/src-tauri/binaries/qaqh-daemon-*.exe`
   （copy 主 worktree 的即可，否则 `cargo check --workspace` 死在 Tauri build script）；
   `cargo fmt -p <crate>` 会顺手重排该 crate 里**无关的**未格式化文件（本仓 `qaqh-runtime` 的
   `ringing/orphan_seal.rs`/`projection.rs`/`service.rs`/`timeline.rs`、`tests/subagent_inprocess.rs` 都有），
   提交前必须 `git checkout --` 掉这些。
7. **既有无关失败**：`agent::prompt::tests::prompt_and_tool_defs_char_budget`
   （`crates/qaqh-runtime/src/agent/prompt.rs:216`，"identity prompt 10001/10093 chars"）。
   与工具面无关，本清理线每次都只挂它一个。

## 三、剩余挂点

**P0 尾巴 ④ — ts-rs 白名单化**（audit 估 156/190 死类型；`Projection*` 需先转正再定）
- 生成物在 `webui/src/api/qaqh/`（当前 185 个 `.ts`）。
- 口径：`just ts-export`（`cargo test` 跑 `export_bindings_*`，`TS_RS_EXPORT_DIR` + `TS_RS_LARGE_INT=number`）；
  `just ts-check`（`git diff --exit-code webui/src/api` 兜漂移）。**纯生成物边界，零运行时风险**，建议先接。

**P1.2 — 删 `ToolEvent::ToolCallPrepared` / `CodeChanged`**（连带重审整张 `DomainEvent` 枚举谁还在桥上被匹配）
+ `RingingEvent` writer 载体换 timeline intent 后删除。audit 注明 fire-into-void 是**整张** `DomainEvent` 枚举，不只两个变体。

**P1.3 — `ToolCallProgress` 改携带片段 + 累计长度**（gate 三适配器）。

**P2 — migrate-on-read 各项**（`compact_skip` / `index.json` / `workspace.txt` / `timeline-v3` / journal 字段 /
`tool_outbox.wal` / DeepX marker / `provider_id` / 明文 key / 扁平 model / 权限 u8 / `normal` alias /
timeline 旧 JSON 槽位；discovery pre-0.9 兼容；`/control/v1/*` 改名）——**每项先写审计查询确认零命中再删**。

**P3 — 独立工程**（不是"旧协议没删"）：canonical 接管 message/journal 写 → 收敛 `LegacyWriterFacade`
双栅栏为单一 `events.lock`；BETA-01 目录名 = canonical id（启用 `rename_session`，退役 seed 目录解析与
`ringing-driver-watch.json` seed 键）；`to_tool_result()` 下游改吃 `ToolOutcome`；spec-file-mutation-delta 开工或归档。

## 四、验证口径

```bash
cargo check --workspace --all-targets        # 期望 0 err
cargo test  --workspace                      # 期望仅 prompt_and_tool_defs_char_budget 一个失败
just ts-export && git diff --stat webui/src/api   # 触及 wire 类型时必须跑；期望无 diff
# 取消语义回归（动 cancel/TLS 时必跑）
cargo test -p qaqh-runtime --test cancel_keeps_tool_results --test concurrent_read_stress --test tool_ordering_contract
```
- 测试串行由 `.cargo/config.toml` 的 `RUST_TEST_THREADS=1` 保证（全局状态共享，别去掉）。

## 五、P1.1-① 的验证证据（本次会话）

- `cargo check --workspace --all-targets`：0 err
- `cargo test --workspace`：仅 `prompt_and_tool_defs_char_budget`
- `qaqh-workspace`：452 lib 测试全绿（含补回的 10 例 `map_tool_result` 契约）
- 取消三件套：`cancel_keeps_tool_results`(4) / `concurrent_read_stress`(4) / `tool_ordering_contract`(6) 全绿
- 隔离 worktree `E:\qaqh-backend-p11`（分支 `refactor/p11-tls-removal`）验证后退场：
  合入用 `git merge --ff-only`，**先核对零文件重叠**（我的 26 文件 vs 同事脏文件清单，交集为空），
  合入后同事脏文件清单逐字节一致；worktree 与分支已删（`git branch -d`，因已合并）。
- **未 push**：`main` 现 `ahead 12`（既有待推状态，非本次产生）。

## 六、接手建议顺序

① ④ ts-rs 白名单（零运行时风险，先清生成物边界）
→ ② P1.3 `ToolCallProgress`（小、独立）
→ ③ P1.2 `DomainEvent` 桥上枚举重审（**先做匹配点普查再删**，audit 明说不止两个变体）
→ ④ P2（每项先审计查询）。
