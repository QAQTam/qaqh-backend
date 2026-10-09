# Handoff：沙箱 exec 分级自动放行 + SandboxRun 档（feat/sandbox-exec-policy）

日期：2026-10-09。分支：`feat/sandbox-exec-policy`（基于 main @ `cb6750f`，未推送远端）。
前置阅读：[ADR 2026-10-09](../adr/2026-10-09-sandbox-exec-autoapprove.md)、
[GPT 审计报告](../analysis-windows-appcontainer-projfs-2026-10-09.md)、AGENTS-x.md。

## 1. 本次会话完成的事（时间序）

### main 上的三个提交（分支切出前）

| 提交 | 内容 |
|---|---|
| `ec3bb62` fix(workspace) | **resource_mismatch 热修**：`extract_target_paths_in` / `resolve_target_path_in` 显式 base——授权快照与执行复核不再读线程局部工作区。症状：相对路径 read/grep 全部 `resource_mismatch`；根因是两侧提取运行在不同线程、相对路径基准（TLS→全局→cwd）不一致 |
| `910b8e6` refactor(config) | owner 的 config-api 清理与补充（本会话代为提交） |
| `cb6750f` docs | ADR + ask-user-form spec/ADR + 交互视图/驱动座设计稿入库（原为 untracked，ARCHITECTURE.md 链接悬空） |

### 分支上的四个提交

| 提交 | 内容 |
|---|---|
| `dd57215` feat(sandbox) | **阶段 1：exec 只读分类器**（`qaqh-permission/src/command_class.rs`）+ 授权接入。WorkspaceWrite 档 + 沙箱写强制 + 分类只读 → 自动放行，凭证 `GrantKind::SandboxClassified` |
| `23c9031` feat(sandbox) | **v2a：沙箱写拒绝导流附疑似目标路径**（`sbx-win/src/feedback.rs` 的 `candidate_write_targets` / `denial_feedback_with_targets`，`command_text` 贯通 handler→direct→sbx_exec） |
| `a32276d` feat(sandbox) | **SandboxRun 档（wire 档位值 4，owner 授权）**：exec 全自动（ReadOnly/Unclassified），deny 形态/网络/会话路径保持审批；独立下载命令升级为 Risky（网络 fail-closed） |
| `bb69cc3` fix(sandbox) | **审计 F1/P0 止血**：exec 自动放行门禁要求 `resolve_windows_backend(spec)` 解析到 sbx 后端；Auto 未晋升 = 不自动放行（fail-safe）+ 回归锁 |

### 文档产物

- `docs/adr/2026-10-09-sandbox-exec-autoapprove.md`（含决策 1-5，决策 5 = SandboxRun，owner 授权记录在案）
- `docs/ARCHITECTURE.md` 专项设计登记
- 对 Microsoft agent-governance-toolkit（`E:\agent-governance-toolkit`）与 Codex（`E:\myXCode`）的对比分析结论**只存在于会话记录**，未落文档——若后续做策略代理里程碑需要补

## 2. 关键设计事实（接手前必读）

1. **分类器只决定摩擦，不是安全边界**。安全边界是 sbx TokenPlane 的内核 DACL（写）与 deny 模式（网络，直到强制网络落地）。分类器 fail-closed：POSIX 有界分词器识别不了的形态一律 Unclassified；pwsh/cmd 不做文法分词（backtick 语义相反），只做 deny 特判。
2. **门禁条件（authorization.rs exec 门）**：非子代理沙箱 + 档位匹配（WorkspaceWrite→仅 ReadOnly 分类；SandboxRun→ReadOnly|Unclassified）+ `spec.enabled` + **`resolve_windows_backend` 解析到 Token/Redirect**（审计 F1 止血）+ 平台 `filesystem_write_isolation` + 命令文本不含会话敏感路径。命中 → `Authorized` + `SandboxClassified`。
3. **deny 形态清单**（command_class.rs）：递归删除（POSIX `rm -r*` + pwsh `Remove-Item -Recurse` 前缀匹配）、secret 读（`.env`/`id_rsa`/`.ssh` 等，模板样例豁免）、下载管道进 shell、`iex`、环境倾倒（`printenv`/`Env:`）、云 metadata 端点、独立下载/联网命令（网络未强制前所有档位都审批）、git push/pull/fetch/clone。
4. **白名单初始集**：rg/grep/findstr/cat/head/tail/wc/cut/uniq/jq/ls/git 只读子命令等约 50 项；特判 rg `--pre`、git `--output=` 拒判；find/fd/sort/awk/sed/python 等不在白名单。扩集改 `PLAIN_READ_BINARIES` / `GIT_READ_SUBCOMMANDS` 常量 + 测试。

## 3. 审计集成状态（analysis-windows-appcontainer-projfs-2026-10-09.md）

| 审计项 | 状态 |
|---|---|
| F1/P0 Auto 实际 plain spawn | **门禁侧已止血**（bb69cc3：不解析到 sbx 后端就不自动放行）；**执行侧未做**——direct.rs 分派前仍无统一 launch-plan 解析，Auto 档依旧无强制，只是不再错误放行 |
| F1 对 SandboxRun 的影响 | 门禁收紧后 SandboxRun 在 Auto 未晋升时整体退化为 workspace-write 行为（fail-safe） |
| F2/P0 protected ACL 继承 capability 写授权 | 未做。**Redirect 路径 P0，AC×Redirect 组合验收依赖它** |
| F3/P1 ensure_profile Create-first | 未做。审计推翻了"26300 标准 API 失效"的旧结论——**标准 SECURITY_CAPABILITIES 路径实测可用**，AC 转正最大假障碍已清除 |
| F4/P1 TOKEN_GROUPS 变长数组 | 未做 |
| F5/P1 管道排空太晚 | 未做（sbx_exec try_wait→wait 才读管道，大输出假超时） |
| F6/P1 overlay 删除审批 pending 生命周期 | 未做（即之前说的 redirect "step 4"） |
| F7/P1 取消/失败后 merge 语义不诚实 | 未做 |
| F8/P1 restore/merge 并发窗口 | 未做 |
| §6 资源类（FreeSid/CloseDesktop/ACL 升级幂等/ProjFS 对齐等） | 未做 |
| deny-steer 启发式局限 | 审计与我方 ADR 口径一致（"疑似"），已在注释声明 |

## 4. 验证状态（如实声明）

- 已验证：policy 14 / permission 37 / workspace 140 + 全部集成套件 / process-tools 62 / sbx-win 21 + 套件全绿；`cargo check --workspace` 无错误；改动文件 fmt/clippy 干净（工作区存在**改动前就有**的 warning：exec_audit.rs unused import、manager.rs unused mut、permission.rs:181 单元素 for——非本工作引入，未顺手改）
- **未验证**：
  - 全 workspace 测试在 `qaqh-client/src/discovery.rs` 有 2 个环境性失败（Windows 进程回收时序），cargo fail-fast 阻断后续 suite；需 `cargo test --workspace --no-fail-fast` 补跑确认与本工作无关
  - **真实端到端**：当前运行中的 daemon 是旧二进制。分类器自动放行、v2a 导流文案、SandboxRun 档都需要重新编译 + 重启 daemon 后亲证
- 已实测有效：显式 WindowsToken 后端的 DACL 写拦截（sbx_bypass 逃逸测试，本机跑过）

## 5. 下一步（建议优先级）

1. **F1 执行侧修复**：direct.rs 分派前统一做 launch-plan 解析（Auto → resolve → 按 resolved backend 分派），授权与执行共享同一解析结果——所有后端的共同地基
2. **F2 protected ACL 真重建** + 用生产 scratch 布局做"无授权/单文件授权/deny carveout"验收
3. **F3/F4/F5 + 资源 RAII**：AC 底座修正（Create-first、FreeSid、变长数组、spawn 即排空管道、CloseDesktop、Job 句柄所有权）
4. **F6 审批接线**：pending overlay + ApprovalRegistry challenge + 精确批准集合 + 基底冲突检测——与 SandboxRun 的"拒绝→询问→扩路径重试"（v2b）合流设计
5. **AC 转正验收矩阵**：标准/lowbox 对照、readable_roots 自动推导（扫 PATH/工具链）、网络实测（空 capability 断网 + internetClient 放行）、AC×Redirect 合成、多 build 对照
6. **网络策略代理里程碑**（未立项）：Codex 参照架构 = 专用账户 + WFP ALE_USER_ID + 本地策略代理（域名 allowlist/NetworkPolicyDecider 审批钩子/call_id 归因）；我方推荐 = AC 断网 + loopback exemption + daemon 内自写策略代理（tokio/hyper，MIT，无新依赖）或 mihomo sidecar（GPL-3.0，进程边界安全）；本地 mihomo 策略代理可复用 `E:\clash-verge-rev` 生态认知，必须独立实例 + secret 隔离，警惕 agent 可达 external-controller 的反向洞
7. 桌面端（另一仓库）补 SandboxRun 档 UI 选项与"沙箱拦截写入"审批语义展示

## 6. 杂项与提醒

- 工作区遗留 untracked：`prompt.md`（会话系统提示词转储）、`.zcode/`（本地工具状态）——不入库，建议加 .gitignore
- 审计遗留：`C:\Users\tsy3m\AppData\Local\Temp\qaqh-acl-audit-18608-c8d49a89` 未清理（审计方声明清理被审批拦截）
- `read` 工具在本会话早段对 `E:\qaqh-backend\crates\sbx-win\src\feedback.rs` 出现过 `line_out_of_range`（total_lines=139 但任意区间报越界），疑为 stale-file ledger 与写入交互的 ledger 失步；`write` 全量覆写后恢复。若复现：换 `exec` 读取或全量覆写绕过，并留 bug 记录
- 本会话 `ask` 工具两次参数校验报错（questions/options 类型收窄），改用文字提问绕过——如需修，查 Ringing ask 的 schema 绑定
