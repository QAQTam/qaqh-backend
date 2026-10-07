# Handoff:权限三档制改造(L1–L4 → read-only / workspace-write / skip-permissions)

> 2026-10-03 续作完成时点。状态:**代码改造完成,`cargo test --workspace` 已拿到
> 一次完整绿灯**(含 ts-export 绑定同步)。唯一未完成项是 §5 提交——被工作树的
> 多任务叠加状态阻塞,详见该节。

## 1. 目标与映射(用户决定,勿回退)

废除旧 L1–L4 四档,改为三档;wire 上仍是裸 u8,数值单调(越大越放行):

| 新档 | 值 | 旧档 | 语义 |
|---|---|---|---|
| `ReadOnly` | 1 | ≈ 旧 L2 ReadFree | 工作区内读自动放行;**一切变更(write/exec/net)逐次审批**。fail-closed 兜底档 |
| `WorkspaceWrite` | 2 | = 旧 L3 WorkspaceFree | 工作区内读写自动;跨工作区写走一次性目录信任;exec/net 逐次审批 |
| `SkipPermissions` | 3 | = 旧 L4 Unrestricted | 普通工具全部自动(含 exec/net);**敏感路径守卫(会话文件/平台 config/skill 根)在任何档位仍强制审批** |

- 旧 L1 MaxLockdown(全审批)**无对应档,整体废除**;其唯一语义松动:最严档下工作区内读不再逐次弹审批。
- `read-only` 采用"读自动、变更走审批"而非 Codex 式硬拒绝——保留 qaqh 审批 UX。
- 沙箱 spec 与档位解耦不变(旁路只是跳过审批,不关沙箱)。
- wire/API 键名保留 `permissionLevel`(数字语义变为新档),避免前端大改。

## 2. 关键设计决定:持久层双键(防升级越权,已实现,勿简化)

数字 3 在旧四档(WorkspaceFree)与新三档(skip-permissions)语义不同。若按数字直读:
旧 3 用户升级后拿到免审旁路 = **越权**;新 3 用户每次 load 被降级 = 无法落盘。因此:

- 新盘键 `permission_tier`(严格 1..=3,save 唯一写出键);
- 旧键 `permission_level` 仅 load 时经 `PermissionLevel::from_legacy_u8` 迁移
  (1/2→1, 3→2, 4→3),save 恒写 `None`(从盘上消失);
- 两键并存时 `permission_tier` 胜;非法值(含新键 4+)fail-closed 钳到 1 + warn。

## 3. 已完成的修改(文件清单)

**定义与裁决**
- `crates/qaqh-policy/src/lib.rs` — 枚举三档重写;`from_u8/try_from_u8/is_valid_u8`
  (1..=3,非法→ReadOnly);`from_legacy_u8` 迁移助手;`as_str()/label()/description()`;
  单测(invalid fail-closed + legacy 迁移)。
- `crates/qaqh-workspace/src/permission.rs` — `needs_permission` 裁决表:
  SkipPermissions 旁路 → 工作区读自动 → `level >= WorkspaceWrite && Write` 走信任目录
  逻辑 → Ask;reason 文案去 Level 化;测试(0..=255 扫描改 1..=3、ask/todo 遍历改三档)。
- `crates/qaqh-workspace/src/authorization.rs` — D5 bypass `level == SkipPermissions`(sed)+ 注释。

**配置/持久层**
- `crates/qaqh-config/Cargo.toml` — 新增依赖 `qaqh-policy`。
- `crates/qaqh-config/src/config.rs` — 默认 `permission_level: 2`;load 双键逻辑(见 §2)。
- `crates/qaqh-types/src/config.rs` — `PersistentConfig.permission_tier`(新)+ `permission_level`(遗留)。
- `crates/qaqh-config-api/src/lib.rs` — `ConfigPatch::validate` 1..=3(含"旧值 4 必须被拒"断言);注释。
- `crates/qaqh-config/src/dto.rs` — apply_patch 注释(1..=3)。
- `crates/qaqh-runtime/src/service.rs` — `config.set_permission_level` 校验 1..=3,错误文案带档名。

**测试夹具/用例(语义适配)**
- `crates/qaqh-workspace/tests/permission_level_fail_closed.rs` — 整文件重写(三档、
  迁移单调性、admit 端到端、read-only 档语义守卫)。
- `crates/qaqh-config/tests/permission_level_fail_closed.rs` — 整文件重写(patch 值域、
  旧值迁移、`config_load_prefers_tier_key_and_validates_range`、非法钳制)。
- `crates/qaqh-config/tests/base_url_preset_guard.rs` — 夹具改 `permission_tier = 3`。
- `crates/qaqh-runtime/tests/permission_lifecycle.rs` — 6 用例适配:
  4 个 read→write(read-only 下写必审,保留审批生命周期被测语义;risk 断言 Low→Medium)、
  mixed 用例档位 2→1(读自动+写审批)、skill 激活改为自动放行(删审批步骤,
  H2 注入防护在"写 skill 目录"不受影响)。
- `crates/qaqh-runtime/tests/ask_user_lifecycle.rs` — 2 场景 read→write。
- 旧 L4 夹具 4→3:`cancel_keeps_tool_results / session_lifecycle /
  workspace_change_injection / tool_ordering_contract / tool_ledger_{wiring,open_intent,finish_failure}`
  (config 与 authorize_call 两类);`audit_ledger.rs` 断言 4→3(2 处)。
- 全仓 sed 改名(ReadFree/WorkspaceFree/Unrestricted/MaxLockdown → 新变体),波及
  `qaqh-workspace` 的 file_glob/file_query/grep_tool/web/file_mutate/exec/tool_api/*、
  `qaqh-runtime/src/agent/context.rs`(夹具 3→2)、`qaqh-subagent/src/lib.rs` 等。
- `qaqh-workspace/src/runtime.rs` — `ToolCtx::admitted()` 4→3。
- **注意**:`permission_lifecycle.rs:745` 的 `assert_single_completion(&events, 4)` 是
  结果计数不是档位,勿改。

## 4. 续作清单(2026-10-03 已全部处理,残留仅 §5 提交)

1. **✅ 完整绿灯已拿到**:`cargo test --workspace` 退出码 0。定向修复过程发现的
   残留失败点(§3 清单之外,均是"sed 只改变体名没改数字夹具/旧语义断言"漏网):
   - `qaqh-workspace/src/execution.rs` 内嵌测试 9 个:`admit(inv, 4)`/`set_context(…, 4)`
     期望 bypass 的数字夹具 → 3;`admit(inv, 1)` + Read 类 `test_counter` 期望审批的
     → 换 Write 类 `test_write`(旧 L1"最严档全审批"已废除,工作区内读在
     read-only 档自动放行);其中 2 个真正执行 handler 的用例还需给
     `test_write`(risk=Destructive)带上工作区内 `path`,否则被 SafetyPolicy
     缺 path 闸门拦下;`max_lockdown_requires_approval` 改名
     `read_only_tier_requires_approval_for_write`。
   - `qaqh-workspace/src/authorization.rs` D5 测试:`[1,2,3]` 审批循环 → `[1,2]`,
     单独的 `4` bypass → `3`;read 快路径 `[1,2,3,4]` → `[1,2,3]`;测试名去
     `unrestricted`。
   - `qaqh-workspace/tests/dynamic_registration.rs` 同 D5:`[1u8,2,3]` → `[1u8,2]`、
     `4` → `3`。
   - `qaqh-workspace/tests/audit_ledger.rs` ③ PLAN 用例的 `ToolCtx
     permission_level: 4` → 3(4 钳 1 后 edit 在 admit 阶段就要求审批,到不了
     PLAN 检查)。
   - `qaqh-workspace/tests/skills_typed_output.rs` `permission_level: 4` → 3。
   - 旧档名注释/断言消息清扫:execution/authorization/permission/manager/
     audit_ledger/dynamic_registration/todo_contract(qaqh-workspace)+ config-api
     一处 "Level 4" 文案。`qaqh-policy` 迁移表注释保留(刻意)。
2. **✅ webui 展示已做(方案 a)**:`qaqh-domain/src/timeline.rs`
   `TimelineToolPermission` 加 `#[serde(default)] level_name: String`(旧记录反
   序列化为空串);`qaqh-runtime/src/agent/engine_tool.rs` 两处构造点取
   `PermissionLevel::from_u8(config.permission_level).as_str()`;ts-export 再生成
   `webui/src/api/qaqh/TimelineToolPermission.ts`(+7 行,零漂移);
   `webui/src/approval/ApprovalCards.tsx` 显档名(空串兜底 `level N`);
   `webui/src/session/reducer.ts` `normalizePermission` 补拷 `level_name`。
   **注意**:后两个文件混有同日 render-perf 任务的未提交改动,见 §5。
3. **✅ 脚本已改**:两个 probe 脚本写的是**遗留键**,直接改数字会被
   `from_legacy_u8` 二次迁移压扁语义,故换新键:compact-probe
   `permission_tier = 2`;content-probe `permission_tier = 1 if MODE ==
   "permission" else 2`。
4. **✅ 文案清扫完成**(见 1);`docs/current/architecture.md` 不存在,第 6 项豁免
   (三档权威定义在 `qaqh-policy` 文档注释 + 本文档)。
5. **⛔ 提交被阻塞,需用户决策**:handoff 写作时工作树约 45 个 M 文件、按 §3 清单
   圈文件可行;现在 `git status` 有 **516 个改动文件**,是同日并行任务(至少
   webui render-perf,见 `docs/handoff/webui-render-perf-2026-10-03.md`)与本任务
   的未提交改动叠加,且互相渗入同一文件(`timeline.rs` 混着失败槽投影
   `tool_failure_of`,`engine_tool.rs` 92 行变更中本任务只占约 12 行,
   `ApprovalCards.tsx`/`reducer.ts` 同理)。文件粒度圈选会把其他任务的工作以
   本任务名义提交。可选路径:a) 先提交 render-perf 等任务,再圈本任务文件;
   b) 接受混入,按 §3+§4.1/4.2 清单整文件提交;c) hunk 级拆分(成本高)。
6. 已知 flaky(与本任务无关,单独重跑即绿):
   `qaqh-runtime ringing::hub::tests::offload_page_keeps_shell_when_sidecar_is_missing_or_stale`。

## 5. 验证锚点(改动后必须复跑)

- `cargo test --workspace`
- `crates/qaqh-workspace/tests/permission_level_fail_closed.rs`(三档语义守卫)
- `crates/qaqh-config/tests/permission_level_fail_closed.rs`(双键/迁移/钳制)
- `permission_lifecycle` + `ask_user_lifecycle`(审批生命周期)

## 6. 并行工作备忘(与本次无关,但同工作树/同会话)

- `E:\win-sandbox-rs`(独立仓,已三次提交,23 测试绿):Windows 沙箱 TokenPlane,
  M1 完成,待合入 qaqh-backend(方案见该仓 README/ADR-0001 与
  `docs/archive/windows-sandbox-cross-review.md`)。合入时的 `write_paths` 审批接线
  将消费本次三档制的 `writable_files` 语义。
