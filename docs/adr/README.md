# ADR 台账

本目录是**后端契约与分支模型的决策记录**。`Cargo.toml` 中 `undocumented_unsafe_blocks = "deny"`
的注释早已指向 `docs/adr`，本目录即其落点。

## 什么情况必须写 ADR

1. 任何触及 `docs/lts-v3.0-maintenance.md` §1 冻结面的提案 —— **被否决也要写**，否则同一提案会反复重提；
2. 任何机器闸门的**新增或退役**（CI job、守卫脚本、导出工件）；
3. 任何"为了兼容再加一层转换"的念头被接受或被拒绝时（项目宪法第 9 条要求留痕）。

## 状态词表

| 状态 | 含义 |
|---|---|
| `已采纳` | 决策定稿 **且代码已落地** |
| `已采纳（未实施）` | 决策定稿，代码未动。**禁止把此状态读成"已完成"** |
| `提议中` | 等待拍板 |
| `已拒绝` | 明确不做的方案，防止反复重提 |
| `已取代 by ADR-xxxx` | 旧决策作废，保留以追溯 |

## 模板

```
# ADR-NNNN <标题>
状态: <词表之一> · 决策日期 · 决策人
## 背景        —— 只写已被代码验证的事实，附 file:line
## 决策        —— 祈使句，一句一个可验收的动作
## 被否选项    —— 选项 + 为什么不做（这是 ADR 的主要价值）
## 后果        —— 代价、失败模式、对本仓库现有闸门的冲击
## 落地清单    —— 未实施时逐条可勾
```

## 索引

| 编号 | 标题 | 状态 |
|---|---|---|
| [0001](0001-kernel-userspace-branch-model.md) | kernel/user-space 分支模型与 LTS API 冻结 | 已采纳 |
| [0002](0002-contract-source-of-truth-no-openapi.md) | 契约真相源 = Rust DTO；拒绝 OpenAPI 承载 RingingV2 | 已采纳（含 `已拒绝` 方案） |
| [0003](0003-route-registry-fail-closed.md) | 冻结守卫必须 fail-closed：路由 registry 单一真相源 | **已落地于 main**（表驱动 + 删正则 + 禁止型 lint + 双向覆盖闸 + 行为测试）· r2 修订：registry 本就存在 |
| [0004](0004-wire-compat-classes.md) | wire 兼容方向是声明数据，不是隐含 serde 行为 | **已采纳（部分实施）** · §待决**已决**：出站引入 `unknown` fallback（入站维持 reject）· 落地清单**前两条 fail-open 已落 `main`**（契约盲区入导出物 + `.ok()` 改显式失败） |
| [0005](0005-n-minus-one-pairing-tests.md) | 兼容性由 N-1 配对测试证明，不由字节相等证明 | 已采纳（未实施） |
| [0006](0006-contract-as-versioned-package.md) | 契约分发为版本化包 + provenance | 已采纳（未实施） |
| [0007](0007-refreeze-governance.md) | 换锁治理：CODEOWNERS + refreeze/tightening 标签 + 守卫自保护 | **已落地于 main** + **LTS 分支保护已开**（缺口表见手册 §7） |
| [0008](0008-neutral-ir-per-language-projection.md) | 中立 ContractIR + 每语言投影，退役 ts→cs 解析链 | **第 1 步 + 第 2 步的量尺已落地**（`schema-preview` + `ir-diff`）· 替换投影与删解析机未实施 · §第 2 步（量）记着实测：ts-rs 无结构化 IR ⇒ 原计划"双跑逐字节 diff"改为属性级对账 |
| [0009](0009-wire-view-vs-artifact-bytes.md) | 冻结粒度区分 wire 语义与产物字节：分类从两类改三类 | **已落地于 main**（`wire_view` + `doc-only-refresh`）· 由真实误判触发 |
| [0010](0010-cut-lts-v3.1-line.md) | 换锁：v3.1 线的待拍板取值与攒着的 refreeze 清单 | 已采纳 · 已实施（2026-11-13 所有者指令切线 LTS-v3.1 + v3.1.0 发版；revision 取 commands-9/session-2，裁决与实测见 §切线实施记录；未实施条目留 main 演进归 v3.2 账本） |
| [0011](0011-frozen-lock-identity.md) | 冻结线的锁必须有登记身份：main 上「改金标准让它变绿」这条路从此被封死 | **已采纳（agent 落地于 main，待 owner 复核）** · 反证已做 |
| [0012](0012-exec-pwsh-uutils-preamble.md) | exec 的 pwsh 壳集成 uutils coreutils：探测式 preamble 注入 + 最小别名手术 | **已采纳（代码已落地）** · vendor 分发与引导下载延后（另行 ADR） |
| [0013](0013-legacy-history-graft.md) | 旧史嫁接：QAQ-Harness 298 提交并入 qaqh-backend（1387 连续历史） | 已采纳 · 已执行并 force-push · commit-map 与 gpgsig 陷阱见正文 |
| [0014](0014-subagent-as-session.md) | 子代理=会话：单权威收敛与跨会话消息（collector/双身份机构退役，wire 不变） | 已采纳（部分实施）· 切片 A–G 已落地；决策 4 超时扫描已取代 by ADR-0018 |
| [0015](0015-goal-mode.md) | Goal 模式：持久会话目标 + 空闲自动续跑 + 预算熔断（Codex 语义，ADR-0014 机构承载） | 已采纳 · 切片 A–E 代码落工作区（待提交）；wire 实变已记 ADR-0010，revision -9 |
| [0016](0016-handoff-kernel-fork.md) | Handoff 内核 fork：封箱续推归内核，前端退化为纯投影（翻转 0015 切片F「内核不代建会话」的机器续推部分） | 已采纳 · 切片 0–4 已实施（封箱闭门/内核 fork 引擎/HandoffSealed+血缘导出/daemon sweep/前端纯投影化,证据见落地清单）；运行半边未跑；wire 实变（HandoffSealed + 血缘导出）已记 ADR-0010；决策 3 经所有者 2026-11-13 复核确认,决策 6 [立即继续] 口径统一中 |
| [0017](0017-exec-full-output-terminal-lifecycle.md) | exec 完整输出与只读终端生命周期：持久化 offset 日志 + 区间读（不引 WebSocket）；附加决策：移除 exec 终态唤醒注入链（已实施）、revision 不 bump | 已采纳（部分实施） · 切片 0 后端半 + 切片 1 已实施（append-only 日志、`exec_session.output` 区间读、DTO 导出，证据见落地清单）；切片 0 前端半随前端仓切片 2、切片 2/3 未实施；决策 2 result 形状修订为单 `text`（实施时定夺）；wire 实变（`exec_session.output` + 2 DTO）已记 ADR-0010 |
| [0018](0018-subagent-terminal-truth.md) | 子代理终态说真话：掐流=失败、预算与父同律、配置面退役（取代 0014 决策 4） | 已采纳 · 投影/gate/清扫/配置面/迁移已落地；wire 实变（ConfigDto/ConfigPatch 删 subagent 段 + 两型）已记 ADR-0010 |
| [0019](0019-team-inbox-inter-agent-actor.md) | Team inbox 跨根署名：`InterAgentActor` 寻址统一（path/session union），跨根投递入 inbox | 已采纳 · 已实施（2026-11-13 随 LTS-v3.1 切线：读侧 + 死变体退役，含孤儿 InterAgentContent/MAILBOX_TEAM）；wire 实变由 v3.1 金标准机器重切吸收 |
| [0023](0023-retire-inline-think-split.md) | 退役 gate 的 inline `<think>` 兼容切分：正文只认端点声明的 reasoning 字段 | 已采纳 · 代码已落工作区，发布/标注见落地清单未完成项 |
| [0024](0024-home-control-mcp-transport.md) | 硬件控制 MCP：Rust 独立进程 + stdio + 2026-07-28 无状态协议 | **已采纳（部分实施）** · server 侧真机闭环（2026-11-17：四工具 + 策略层 + 云侧读写 + 审计/幂等/冷却/带外批准/会话探活与静默续期(默认关)；`cargo test` 35 绿 + rmcp Auto 客户端集成测试 3 绿、stdio 冒烟 50 断言绿（含 era 反证））；**daemon/`qaqh-mcp` 侧未接线**，零 wire 变化验证未跑；era 反证两半边都已用 rmcp 客户端/手搓管道验证，但**未过真实 daemon** |
| [0025](0025-mcp-default-on-and-third-party-compat.md) | MCP 默认开与三方兼容（client 侧）+ rmcp 统一 3.10.0 | 已采纳 · 切片 A–D + R1（精确锁 =3.10.0）+ R2（被否：不砍 qaqh-skills 复用 MCP Skills）已留痕；延后项见落地清单 |
| [0026](0026-exec-notify-on-exit-subscription.md) | exec 订阅终态推送：`write_stdin notify_on_exit`（订阅制回调决策 8；armed+delivered 恰一次交付、注入不带正文、Exited/TimedOut 可推送白名单） | **被取代（ADR-0029）** · "竞态归零"声称被证伪（入队≠消费，残余窗口=整个活跃回合）；机构整体砍除 |
| [0027](0027-v4-0-gold-standard-recut-for-tool-image-ref.md) | 在 `main` 上就地机器重切 v4.0 金标准，追认 `ToolImageRef` + `TimelineEntry.images` | **已采纳 · 已实施**（2026-11-17 所有者裁定：用户空间不可变只在 LTS 仓、当前无 LTS 发版计划）· `check` 实测 exit 1→0 · 已知不严格：`ringing-commands-10`/`ringing-session-3`/`wire_version 2` 三戳**未 bump**，禁止当成 refreeze 免记账的先例 |
| [0028](0028-user-shell-terminal-on-backend.md) | 用户 shell 终端落后端：复用 `qaqh-pty`，前端 node-pty 退役；数据面走 ws | **已采纳 · 切片 1+2 已实施（实测：pty 2 passed / runtime shell 3 passed / runtime lib 54 passed）· 切片 3–5 未实施** · 生命周期考据：断联**不** kill（`ExecStopReason` 无 client 档），owner=`instance_key` 而非 `session_id`，idle 回收挂既有 60s sweep · **§D3 已裁定 = B（ws）并显式 supersede ADR-0017 决策 6 与§被否选项第 4 条**，ADR-0017 头部加了指回注记；候选 A（长轮询 offset 区间读）留作显式回退 · §更正记录列出第一版 7 处未实测引用 |
| [0029](0029-retire-exec-notify-single-channel-long-poll.md) | 砍掉 exec 订阅终态推送，回归单通道长轮询（取代 0026；codex unified_exec 同款：空 poll 5s–300s、非空 ≤30s，终态只经读取通道） | 已采纳 · 机构整体砍除（manager/工具面/桥/权限/registry/service/daemon/模板/skill）；验证结果见落地清单；git fetch 比对受限已登记 |
| [0030](0030-exec-output-delta-pump-coalescing.md) | exec 输出 delta 泵侧合帧：帧频与 DB 写放大的源端上限（P0 修复 F 第一半） | 已采纳并实施（经 0034 承接，2026-11-20）· 泵侧 20ms/256KiB 合帧批，wire 零变更；折叠/熔断、前端降级 pull、exec ws 隔离流仍为非目标 |
| [0031](0031-tag-not-lts-release-model.md) | tag ≠ LTS：发版与冻结线解耦（规则 15/21 松绑修订） | 已采纳 · refreeze 在 main 的去处=新版本位（minor bump + 机器重切），不再强制开 LTS；三戳只随命令/事件词表与 wire 协议实变 bump；LTS-v3.x 纪律不动 |
| [0032](0032-cut-lts-v4.1-line.md) | 切 LTS-v4.1 线：新一期 LTS 开发，金标准等机器按需恢复（真源 `da33cbae^`） | 已采纳 · 分支 `LTS-v4.1` 自 main `bb28211b` 切出，线版本 4.1.0；tag 待线就绪另发 |
| [0033](0033-agent-team-2-colleagues-over-sessions.md) | AgentTeam 2.0：同事=会话，一块任务板一条通道 | 已采纳并实施（切片 A/B/C+D 全落 main，见落地清单；`[contract-refreeze]` 三戳 bump + v4.0 金标准重切 EXIT=0）· 冒烟待真实模型环境 · 前端同批（姊妹仓）未动 |
| [0034](0034-exec-flood-memory-surface.md) | exec 洪峰内存面：源端水位 Arc 共享 + 全文权威源=日志文件（P0，修订 0017 决策 3 写入面） | 已采纳并实施 · reader 64KiB + 同 run 合帧 + `ExecOutputWatermark(Arc<str>)`；settle 不读日志内容，对账降级 `output_complete`，content 复制废除；wire 形状零变更 |
| [0035](0035-exec-flood-model-spill-gate.md) | exec 洪峰模型消费面泄洪闸：超限转引用 + 不恢复 + 阈值硬编码 | 已采纳 · 未实施（2026-11-22 所有者四项拍板）· 闸门装 session_loop/tool_job 消费面、客户端 SSE 不设闸；承接 0034 权威日志为泄洪区；熔断降为分级响应第三档仍开放 |
| [0036](0036-remove-tokenizers-facility.md) | 移除 tokenizers 库与全套 tokenizer 配置设施 | 已采纳并实施 · `[contract-refreeze]` 4.0.2→4.1.0 + v4.1 金标准机器重切 EXIT=0；tokenizerPath wire 字段删除；count_tokens 启发式当时保留（0037 推翻） |
| [0037](0037-exec-output-metering-chars.md) | exec 输出计量 tokens→chars：本地 token 估算层退役 | 已采纳并实施 · wire 零变更（内核空间）；max_output_chars 参数/policy/预算链改名；token.rs/estimate_tokens/total_tokens 删除；记账全靠上游 usage 字段 |
| [0038](0038-lifecycle-hooks.md) | 生命周期 hooks:17 事件词表+五门+独立信任存储+wire 事件面 | 已采纳并实施(`[contract-refreeze]`:4.1.0→4.2.0 minor bump + v4.2 金标准机器重切 + check EXIT=0)· SESSION_CONTRACT_REVISION ringing-session-4→5 · HookStarted/HookFinished/PromptBlocked 三帧 ProviderRetrying 同律 |
| [0039](0039-unify-tls-crypto-provider-aws-lc-rs.md) | TLS/密码学栈统一:全图单 provider aws-lc-rs,host 构建图移除 ring(client target 分治,ohos 保 ring;dsaccount/siwc/rcgen 弃 ring;非契约面) | 已采纳并实施(`-p` 定向测试 60+65+84+7 全绿;host 编译单元 ring=0;lock 保留 ohos-only ring 条目) |
| [0040](0040-windows-sandbox-preflight-and-escalation-permission.md) | Windows 沙箱分步自检协议(`sandbox.preflight.start/status`)、`exec_command` 接入 `sbx-win` 执行会话、`with_escalated_permissions`+`justification` 提权摘要与 `PermissionGrantScope`(`once\|session\|persistent`)本对话默认授权 | 已采纳并实施(`[contract-refreeze]`:`ringing-commands-11`/`ringing-session-6` + v4.2 金标准机器重切) |
| [0041](0041-qaqh-gitx-worktree.md) | qaqh-gitx:工作区 git 消费(`gitx.snapshot`)与 linked-worktree 会话隔离(`worktree_list/create/remove/bind/unbind`,注册表+owner CAS,双锚点) | 已采纳并落 main(自 LTS-v4.1 e8d6ae6d 移植,ADR 顺延改号 0033→0041 防与 AgentTeam 撞号;wire `qaqh.ringing.gitx/v1` additive;金标准重切随 v4.2 批次) |
| [0042](0042-ringing-v3-rust-native-ipc-and-quic.md) | RingingV3：Rust Native 二进制多路流（`postcard` + 本地 Named Pipe/UDS `yamux` + 远端纯 QUIC `quinn`），冻结 `ADR-0008` C# 投影演进并规划 `v4.8` 物理删除旧 HTTP+SSE+WS 控制面 | 已采纳 · 未实施（2026-11-25 所有者拍板：默认前端切 `../qaqh-gpui`，远端走纯 QUIC 不套 HTTP/3，拒绝 V2/V3 双栈转换层） |

## 阅读顺序

0001 定义"什么不许变"；0002 定义"以什么为真相源、拒绝什么外框"；
0003/0004 修现有闸门的**失效模式**（fail-open、语义未声明）；
0005 把证明方式从"字节相等"升级为"行为配对"；0006/0007 管分发与治理；
0008 是 0003/0004/0005 的承载体 —— registry 与 compat 声明最终都长在 IR 上；
0010 是 0007 那条"换锁只能走新线"的**账本**：main 上攒了几笔 refreeze、切线时要不要 bump
代际、由谁拍板，都记在那里。
0011 补 0007 剩下的最后一个缺口：分类闸只看"改没改金标准",看不见"改了契约却没动金标准",
于是 main 上那条 report-only 的红可以被"重切一下"抹掉 —— 锁的身份从此必须登记。
