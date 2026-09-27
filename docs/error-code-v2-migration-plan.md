# PLAN：qaqh 错误码 v2 化迁移 + bugent 遗留修复

> 生成：2026-09-27。执行者建议：bugent（已验证可用，见附录 A）。
> 本文档自包含：所有路径、验收标准、风险均已内联，开新会话直接投喂即可。

---

## 0. 背景与根因（已确认，勿重复排查）

**现象**：agent 调工具报
`LEDGER_WRITE_FAILED: canonical fact validation failed: invalid error.code: must be stable snake_case ASCII`

**根因链**：
1. 工具结果写 canonical fact 失败（`ToolRunOutcome::LedgerFailed`）
2. 失败后回填 `ToolResult::error_with("LEDGER_WRITE_FAILED", …)` —— **大写码**
3. `qaqh-session/src/session_fact_v2/validation.rs:832 validate_error_code()`
   要求 code 仅为 `[a-z0-9_]` → 大写码被拒
4. 错误 fact 本身落盘失败 → 嵌套报错

**系统性问题**：`qaqh-types::ToolResult::error_with(code: &str)` 接受裸字符串，
全库存在 13+ 处大写码发射点、79 处引用。v2 设计（base spec §7，已实现于
`qaqh-workspace/src/tool_api/error.rs` 的 `ToolErrorCode` newtype + `ToolErrorKind`
封闭枚举）被 runtime 的旧裸字符串路径架空。

**另有一个文法不一致 bug**：`validate_error_code` 只允许 `[a-z0-9_]`（拒绝 `.`），
而 `ToolErrorCode::parse` 的规范文法是
`^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`（允许命名空间段如 `mcp.rate_limited`）。
合法的新式码会被 fact 边界误杀。**validator 必须改为与 `ToolErrorCode::parse` 同文法。**

---

## 批次 1（已完成 ✅）：直接致病码小写化

以下已改并通过 `cargo check -p qaqh-runtime -p qaqh-workspace` +
`cargo test -p qaqh-runtime --test tool_ledger_finish_failure`（1 passed）+
`cargo test -p qaqh-workspace ask_user`（14 passed）：

| 文件 | 改动 |
|---|---|
| `crates/qaqh-runtime/src/agent/tool_runtime.rs` L198/L272 | `LEDGER_WRITE_FAILED` → `ledger_write_failed` |
| `crates/qaqh-runtime/src/agent/tool_runtime.rs` L1008/L1278 | `TIMEOUT` → `timeout`（发射+比较两处） |
| `crates/qaqh-runtime/src/agent/engine_tool.rs` L769 | `LEDGER_WRITE_FAILED` → `ledger_write_failed` |
| `crates/qaqh-runtime/src/agent/engine_tool.rs` L133/836/876 | `TOOL_EXECUTION_FAILED` → `tool_execution_failed`（TimelineFailure） |
| `crates/qaqh-workspace/src/ask_user.rs` L45/59/419 | `INVALID_QUESTIONS`/`EMPTY_QUESTIONS` → 小写 |
| `crates/qaqh-runtime/tests/tool_ledger_finish_failure.rs` L230 | 断言同步 |

⚠️ 注意：这些是小写字面量，尚未收敛到 `ToolErrorCode` 类型（批次 2）。

---

## 批次 2：类型收敛（核心批次）

### 2.1 validator 文法对齐（先做，独立可交付）
- `qaqh-session/src/session_fact_v2/validation.rs` `validate_error_code()`：
  改为调用/复刻 `ToolErrorCode::parse` 的文法（允许 `.` 命名空间段，段首小写，
  段内 `[a-z0-9_]`）。**注意 crate 依赖方向**：session 不应依赖 workspace——
  把文法常量/函数下沉到 `qaqh-types`（或 session 内复制并加一致性测试互锁）。
- 新增测试：`mcp.rate_limited` 等 namespaced 形态必须通过 fact 校验。

### 2.2 发射点收敛到 ToolErrorCode（约 13 处）
| 现码 | 去向 |
|---|---|
| `TOOL_ERROR`（`qaqh-types/src/tool_result.rs:345`，`ToolResult::error()` 默认码） | `tool_error` |
| `NOT_FOUND`（`tool_result.rs:637`） | `not_found` |
| `MISSING_QUESTION`/`INVALID_QUESTION_ID`/`DUPLICATE_QUESTION_ID`/`INVALID_OPTION`×2/`INVALID_OPTIONS`/`DUPLICATE_OPTION`/`UNANSWERABLE_QUESTION`（`ask_user.rs`） | `ask_user.missing_question` 等 Custom 命名空间码，或降为 builtin kind |
| `NO_MATCH`（`tool_side_fold.rs:296`） | `fold.no_match` |
| `WORKSPACE_MISMATCH`（`execution.rs:116`） | `workspace.mismatch` |
| `TOOL_FATAL`（`execution.rs:346`，FatalToolError） | `tool_fatal` |

原则：**内置语义走 `ToolError::new(kind)`（code 由 `builtin_code()` 推导），
工具特有走 `ToolError::custom(ToolErrorCode::parse("ns.code")?)`**。
禁止运行时动态拼接（`format!("{}_failed", name)` 是 stability 天敌）。

### 2.3 比较端与测试同步（约 40 处）
- 全库 grep 上述旧大写码（含 `file_mutate.rs:93` 的 `ends_with("_NOT_FOUND")`、
  `file_glob.rs:431/436`、`grep_tool.rs:651/654/682`、`edit/tests.rs`、
  `process_inspect.rs:315`、`tool_api/legacy.rs:450/677`、
  `tests/audit_ledger.rs:208`、`qaqh-message/src/store.rs:2468/2489` 等）。
- `tool_api/legacy.rs` 的 `from_legacy` 大写映射**保留**（旧会话回放兼容，
  读路径只进不出：映射到 kind 后重新序列化必为小写）。
- `legacy.rs:677` "legacy code 原样保留" 测试语义重新表述为"读入视图保留"。

### 2.4 兜底链不可失败
- `tool_runtime.rs` / `engine_tool.rs` 的 `ToolRunOutcome::LedgerFailed` 兜底：
  code 改用编译期常量（如 `ToolError::LEDGER_WRITE_FAILED` 关联常量或 enum variant），
  结构上不可能非法。
- 兜底写失败**不得再走同一个 `push_tool_result_canonical`**：
  降级为本地 sidecar 落盘 + turn 标记 `indeterminate`
  （`tool_ledger` lease 机制已存在，工具可能已执行，retry 语义保持保守）。

### 验收标准（批次 2）
- [ ] `cargo test --workspace` 全绿（Windows 上预存环境性失败以 HEAD 基线为准，不得新增）
- [ ] 全库扫描 `error_with\(\s*"[A-Z]` 与 `code:\s*"[A-Z][A-Z_]+"` 命中数为 0（legacy 映射函数体除外）
- [ ] namespaced 码 `mcp.rate_limited` 能通过 fact 校验并落盘
- [ ] 回放包含旧大写码的 session segment 不报错，投影内码已归一

---

## 批次 3：防回归 + 文档

1. `ToolResult::error_with` 签名收 `ToolErrorCode`（或加 `debug_assert` + 单元测试）。
2. CI 扫描测试：源码中出现 `error_with("X` 且 X 含大写 → 失败（白名单：legacy 映射）。
3. base spec §7 增补："error.code 属于 schema/ABI，新增码需登记；stable snake_case
   ASCII；Custom 必须命名空间"。文法与 `ToolErrorCode::parse` 互为镜像。
4. spec 与 `validate_error_code` 加互锁一致性测试。

---

## 附 A：bugent 侧状态（执行者）

**已完成且已验证（未提交，在工作区）**：
- CRLF：read_file 剥行尾 `\r`；edit_file LF 空间匹配 + preferred ending 回写；
  write_file 沿用 CRLF；apply_patch 默认 `preserve-line-endings`
- exec：`bash`→`exec` 改名 + per-call `shell` 参数 + BUGENT_SHELL 钉死守卫 +
  pwsh/powershell/cmd 候选链（MSIX 0 字节 stat 筛除）+ argv 按 kind 映射（pwsh 走
  `-EncodedCommand`）；TUI/权限别名/系统提示词同步
- 输出保真：readCapped 截断不产 U+FFFD；截断文案可操作；路径显示统一 `/`
- 验证：`bun run typecheck` = 0；新增 13 测试全过；失败集合与 HEAD 基线一致；
  端到端冒烟 5/5 通过（商店版 pwsh 实测、cmd 实测、CRLF 真实编辑）

**建议先提交**（两个 commit）：
1. `fix(windows): 文件工具换行归一 + apply_patch 默认 preserve-line-endings + 截断/路径显示修复`
2. `feat(exec): bash 工具改名 exec，per-call shell 与 pwsh/cmd 支持，权限规则别名兼容`

**bugent 遗留（低优先，不阻塞本 plan）**：
- edit 账本 / `expected_hash` 乐观校验 / `confirm_apply` dry-run
- seek-sequence 模糊匹配命中时在结果中明示（fuzzy 透明化）
- MSIX 0 字节别名的 spawn 探活（现仅 stat 筛）
- `agent.shell` 配置键（现 `BUGENT_SHELL` 环境变量已可钉）
- `bash`→`exec` 后 rename `src/tools/bash.ts` → `exec.ts`（纯整理）

---

## 附 B：执行注意事项

1. **qaqh-backend 工作区不干净**：存在先前会话的未提交改动（qaqh-client 的
   client.rs/endpoint.rs/timeline.rs/types.rs、qaqh-runtime 的 service.rs/fs_git.rs
   等 Windows 测试修复）。提交时与错误码改动**分开成独立 commit**。
2. **非幂等命令风险**：`LEDGER_WRITE_FAILED` 发生时命令可能已实际执行
   （本会话实测：脚本报错 5 次但文件已被修改）。修复验证阶段避免用
   追加写/计数器类命令做探测；优先重定向到文件再读。
3. Windows 本机跑 `cargo test --workspace` 有预存环境性失败
   （symlink EPERM、`/tmp` 路径断言等），以 stash 对照 HEAD 基线判定回归。
4. 大输出命令在当前 harness 下易触发 ledger 报错：跑测试统一
   `*> 文件重定向` 后读文件。
