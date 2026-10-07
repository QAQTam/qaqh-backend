# V2 权限层抽离计划（决策引擎出 workspace，机制层不动）

> 状态：提案，待执行。前置事实基于 2026-09-27 代码实证。
> 关联：`docs/plan-context-ownership-v2.md`（ContextService 重构）。两者独立，可并行排期。

## 0. 现状分层（实证）

| 层 | 位置 | 内容 | 状态 |
|---|---|---|---|
| ① 词汇 | `qaqh-policy` | ToolCategory / PermissionLevel / PermissionRisk / PermissionDecision / SandboxSpec（纯类型） | 已就位 |
| ② 决策 | `qaqh-workspace` permission(40K) + authorization(29K) + confirm_apply + pending + conflict + audit | needs_permission / classify_risk / TrustedFolderSet / 审批生命周期 / 审计 | **误位于此，本计划对象** |
| ③ 机制 | `qaqh-sandbox`(822 行) | wrap_command（bwrap/Landlock+seccomp/进程加固）、record_denial、能力上报 | 干净，**不动** |

实证要点：
- sandbox 是活的但薄：全仓仅 4 个调用点（`exec/direct.rs:100/408`、daemon 启动能力上报）；
  强制后端全部 `cfg(target_os="linux")`，Windows 上 Auto 只落 ProcessHardening（弱加固）。
- 决策层在 workspace 里熔了三个异质关注点：工具参数解析（工具契约）、信任持久化
  （会话事实）、审批交互（问用户生命周期）——这是它长到 ~100KB 的根因。
- 审批状态已碎片化三处：workspace 的 pending/confirm_apply、ringing-v2 interaction
  registry、qaqh-policy::ApprovalRegistry（本轮已迁入）。三者是同一件事。

## 1. 目标形状

```
tool_api ──PermissionRequest{category, level, paths}──▶ qaqh-policy 决策引擎（纯函数）
qaqh-policy ──AutoApprove / AskUser / SandboxSpec──▶ 执行层（exec/direct 等）
                    │ AskUser   ──▶ 审批交互域（合拢 ringing interaction + ApprovalRegistry）
                    │ 信任决定   ──▶ qaqh-session（会话事实持久化，与 title/mode 同生命周期）
workspace 只剩 authorize_call 门面：组 Request → 调 policy → 组 SandboxSpec → 分发
```

依赖箭头单向：tool_api → policy ← session(注入信任集)；机制层 sandbox 只吃 SandboxSpec。

## 2. 执行步骤（按风险升序）

### Step 1 — 纯函数搬迁（零风险）
- `needs_permission` / `classify_risk` / path 归一化（resolve_target_path、
  normalize_lexically、path_within_dir、is_sensitive_session_path）→ `qaqh-policy`。
- 签名重构：改吃 `PermissionRequest { category, level, target_paths, trusted }`；
  **TrustedFolderSet 不再在决策函数内部读盘**，信任集作为参数注入。
- 预期 ~几百行；policy 由此获得真正的决策逻辑（现在只有类型）。

### Step 2 — 工具参数解析归位（低风险）
- `extract_target_paths` / `patch_target_paths` / `summarize_permission_action`
  → `qaqh-workspace::tool_api::args`。
- 理由：解析依赖各工具 args 契约，知识跟着工具描述符走；policy 从此不懂具体工具。

### Step 3 — 信任持久化归位（低风险）
- `TrustedFolderSet` → `qaqh-session`，与会话事实（title/mode/skills epoch）同生命周期。
- 决策调用点改为从会话态注入信任集。

### Step 4 — 审批交互合拢（最大刀，先建测试基线）
- authorization / confirm_apply / pending / conflict 的"问用户"生命周期
  与 ringing-v2 interaction registry 合并成一个交互域（候选宿主：qaqh-ringing
  或新 crate qaqh-interaction）。
- **前置**：先跑 permission_lifecycle / plan_review_hook / interaction_* 测试基线
  （这些测试当前均为编译绿、未执行状态，基线先行）。

### Step 5 — workspace 收尾
- 仅保留 `authorize_call` 门面（~百行）：组 Request → policy → SandboxSpec → 分发。
- audit/journal 留守还是随审批域走：执行审计（AUDIT_QUARANTINED 等）属于执行监督，
  **留在 workspace execution 侧**，不随权限走。

## 3. 收益与度量

- workspace 预期减重 ~2.5–3k 行 + 对应内嵌测试；五合一变三合一（工具实现/API/执行）。
- policy 获得可单测的纯决策引擎（无盘 IO、无工具契约依赖）。
- sandbox 不动；机制/决策分离保持。

## 4. 风险与守门点

- `AUDIT_QUARANTINED → Indeterminate` 语义链挂在执行侧审计，勿随 Step 4 误迁。
- trusted-folder 决定跨重启生效，Step 3 迁移时持久化格式/路径不可变
  （对齐 restart_prefix_cache 的教训：字段映射不动）。
- Step 4 涉及 ringing v2 冻结契约（interaction registry / typed verdicts），
  合拢方案需先对齐 v2 wire 契约再动手。
- 每步收尾 `cargo check --workspace --all-targets`；测试补齐节奏按既定策略后置。
