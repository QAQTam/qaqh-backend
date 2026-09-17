# docs — 索引与归档状态

> **索引日期**：2026-09-18（UTC+8）
> **最后复核**：2026-09-18（N-5 权限收口与 B-5 审批动作摘要复核后）
> **基线**：`main` @ `2b472f2`
> **本文件用途**：`docs/` 的唯一索引入口——**已完成什么、什么还在跑、归档去哪找**。
> 目录职责与命名约定见 [`todo/README.md`](./todo/README.md) §6；各目录另有 `以yyyy-mm-dd-标题-*.md作为命名` 说明文件。

## 1. 时间线

| 日期 | 里程碑 | 产物 |
|---|---|---|
| 09-12 | 首批缺陷登记与证据报告：timeline 死锁/快照、多会话 401、exec 管道静默截断、子代理无法拉起、会话 cwd 越界 | [`buglist/2026-09-12-*`](./buglist/)、[`report/2026-09-12-*`](./report/) |
| 09-13 | 隐藏缺陷全量静态扫描（31 条）+ Codex 同问题修法对照 | [`buglist/2026-09-13-hidden-bug-scan.md`](./buglist/2026-09-13-hidden-bug-scan.md)、[`report/2026-09-13-codex-parity-analysis.md`](./report/2026-09-13-codex-parity-analysis.md) |
| 09-14 | timeline 工具块内存放大定案并修复 | [`buglist/2026-09-14-timeline工具块内存放大-buglist.md`](./buglist/2026-09-14-timeline工具块内存放大-buglist.md) |
| 09-15 | 工具层契约重写（Tool SDK v1）spec + plan 立项（**均草案待评审**）；client 系列 7 条登记 | [`spec/2026-09-15-workspace工具层契约重写-spec.md`](./spec/2026-09-15-workspace工具层契约重写-spec.md)、[`plan/2026-09-15-workspace工具层契约重写-plan.md`](./plan/2026-09-15-workspace工具层契约重写-plan.md) |
| 09-16 | 安全/并发/前端渲染审查；apply_patch、edit、read_image 契约登记 | [`buglist/2026-09-16-*`](./buglist/) |
| 09-17 上午 | buglist 全量复核：20 份清单 91 条 → 54 条已修、5 条状态过期、2 条部分、29 条仍在生产、1 条描述与 HEAD 不符 | [`todo/2026-09-17-buglist复核-report.md`](./todo/2026-09-17-buglist复核-report.md) |
| **09-17 16:41–19:41** | **九批次集中修复 + 后续待办收尾**（10 个提交，见 §2.1） | 下方 §2 |
| 09-17 | 工具结果展示层契约 spec 补登（**草案待评审**） | [`spec/2026-09-17-工具结果展示层契约-spec.md`](./spec/2026-09-17-工具结果展示层契约-spec.md) |
| **09-17 第二轮 fix** | **N-5 权限层收口**：Level 4 的 Exec/Net 统一审批；**B-5 动作摘要**：审批弹窗展示 `exec` 命令（后端 `cef3faa`/`eef1231`，TUI `4a795b3`） | [`buglist/2026-09-17-exec工作区外写入未收口-buglist.md`](./buglist/2026-09-17-exec工作区外写入未收口-buglist.md) |

## 2. 已归档（2026-09-17，方案 A）

> 归档采用零风险方案 A：文件保留原路径，归档状态以本索引为准。
> 本轮已先完成 4 份遗留 `open` 清单的状态回写，并为唯一 P0 建档后完成“权限层收口”的工作区修复。

### 2.1 九批次集中修复（2026-09-17）

| 批次 | 主题 | 提交 | 条目 |
|---|---|---|---|
| 1 | 子代理取消链（活表登记 / 取消后不注入 / 取消原因位 / 取消传播 / 状态单调性） | `629637d` (#88) | T-1-1~5 |
| 2 | 安全 P0（fs 路径白名单、Destructive fail-closed、token 不外泄、session_locks 释放） | `238331f` (#93) | T-2-1~4 |
| 3 | `apply_patch` 契约（覆盖守卫、失败文案、歧义警示） | `1705449` (#87) | T-3-1~3 |
| 4 | `edit` 契约（描述收敛到 3 kind、Tier3 拒绝行内片段、第四种诊断） | `61b39d0` (#89) | T-4-1~3 |
| 5 | 计量与超限（图片不计入 token 估算、超限本地 pre-flight） | `006b2b3` (#90) | T-5-1~2 |
| 6 | 热重载前置 `changed()` 守卫 + `list_resources` 三态 | `b4851b0` (#92) | T-6-1~2 |
| 7 | chat 路径 `null→{}` 兜底 + LSP 连接表惰性摘除 | `c627a1b` (#94) | T-7-1~2 |
| 8 | 安全 P1/P2（MCP 并发上限 16、DynamicTool Exec/Net 需审批、auth.mjs 原子写、审计 rotate、账本键归一） | `440a608` (#95) | T-8-1~3 |
| 9 | 清单归档与卫生（过期状态、PARTIAL 更正、ID 撞号消歧、哈希纠正） | `563f1e3` (#91) | T-9-1~4 |
| — | 后续待办收尾（N-1~N-4 / N-6 / N-7） | `3749df1` (#96) | N-1~4/6/7 |

执行记录与逐条验收命令：[`todo/2026-09-17-buglist复核-checklist.md`](./todo/2026-09-17-buglist复核-checklist.md)、[`handoff/2026-09-17-buglist复核九批次执行-handoff.md`](./handoff/2026-09-17-buglist复核九批次执行-handoff.md)

### 2.2 更早批次（已闭环）

| 主题 | 提交 | 关联文档 |
|---|---|---|
| 隐藏扫描补盲区（31 条） | `16f2c39` 等 30 个提交 | [`buglist/2026-09-13-hidden-bug-scan.md`](./buglist/2026-09-13-hidden-bug-scan.md) |
| exec 管道静默截断 / 空输出（EXEC-01a~d） | `30a011b` | [`buglist/2026-09-12-exec管道命令间歇性空输出-buglist.md`](./buglist/2026-09-12-exec管道命令间歇性空输出-buglist.md) |
| 多会话热路径串行化与切会话 401 | `13cb21e` 等 | [`buglist/2026-09-12-多会话高频输出与切会话401-buglist.md`](./buglist/2026-09-12-多会话高频输出与切会话401-buglist.md) |
| timeline 工具块内存放大 | `ea6063c` | [`report/2026-09-14-timeline工具块内存放大-report.md`](./report/2026-09-14-timeline工具块内存放大-report.md) |
| client 系列：TS 特性编译、keepalive、陈旧 discovery、深翻页、启动期探测挂起、epoch 归零 | `4ac2f9c` / `d9fa81c` / `9556aec` / `572f36a` / `a9531ce` / `674742f` / `a72ce0c` | [`buglist/2026-09-15-*`](./buglist/) |
| read_image 400 系列 | `1c13662` | [`buglist/2026-09-16-read_image连发触发400-buglist.md`](./buglist/2026-09-16-read_image连发触发400-buglist.md) |

> 闭环判定依据：[`todo/2026-09-17-buglist复核-report.md`](./todo/2026-09-17-buglist复核-report.md) §4「已核实修复在位、状态正确、可直接归档的 54 条」+ §2.1 九批次验收。

## 3. 归档区（已完成 → 指向这些文件）

> 以下条目自 2026-09-17 起视为已归档；文件未移动，避免破坏文档间相对引用。
> `docs/archive/` 目录未创建。

### 3.1 buglist（17 份）

- [2026-09-12-timeline快照落后被当权威装载-buglist.md](./buglist/2026-09-12-timeline快照落后被当权威装载-buglist.md)
- [2026-09-12-timeline死锁与debug桥token泄露-buglist.md](./buglist/2026-09-12-timeline死锁与debug桥token泄露-buglist.md)
- [2026-09-12-多会话高频输出与切会话401-buglist.md](./buglist/2026-09-12-多会话高频输出与切会话401-buglist.md)
- [2026-09-13-hidden-bug-scan.md](./buglist/2026-09-13-hidden-bug-scan.md)
- [2026-09-15-daemon启动期工具探测可挂起-buglist.md](./buglist/2026-09-15-daemon启动期工具探测可挂起-buglist.md)
- [2026-09-15-keepalive存活信号无出口-buglist.md](./buglist/2026-09-15-keepalive存活信号无出口-buglist.md)
- [2026-09-15-timeline深翻页缺失-buglist.md](./buglist/2026-09-15-timeline深翻页缺失-buglist.md)
- [2026-09-15-ts特性编译不过-buglist.md](./buglist/2026-09-15-ts特性编译不过-buglist.md)
- [2026-09-15-热重载吞首次变更与资源文案-buglist.md](./buglist/2026-09-15-热重载吞首次变更与资源文案-buglist.md)
- [2026-09-15-陈旧discovery永久卡死shell-buglist.md](./buglist/2026-09-15-陈旧discovery永久卡死shell-buglist.md)
- [2026-09-15-频道流epoch未归零-buglist.md](./buglist/2026-09-15-频道流epoch未归零-buglist.md)
- [2026-09-16-anthropic-400-真因-上下文超限-buglist.md](./buglist/2026-09-16-anthropic-400-真因-上下文超限-buglist.md)
- [2026-09-16-apply_patch静默覆盖与误导性文案-buglist.md](./buglist/2026-09-16-apply_patch静默覆盖与误导性文案-buglist.md)
- [2026-09-16-edit工具行内片段与kind虚报-buglist.md](./buglist/2026-09-16-edit工具行内片段与kind虚报-buglist.md)
- [2026-09-16-read_image连发触发400-buglist.md](./buglist/2026-09-16-read_image连发触发400-buglist.md)
- [2026-09-16-图片base64计入token估算致压缩空转-buglist.md](./buglist/2026-09-16-图片base64计入token估算致压缩空转-buglist.md)
- [2026-09-17-子代理取消后复活-buglist.md](./buglist/2026-09-17-子代理取消后复活-buglist.md)

### 3.2 report（11 份）

- [2026-09-12-exec输出静默截断与引入点考证-report.md](./report/2026-09-12-exec输出静默截断与引入点考证-report.md)
- [2026-09-12-timeline快照落后被当权威装载-report.md](./report/2026-09-12-timeline快照落后被当权威装载-report.md)
- [2026-09-12-timeline持久化死锁与debug桥token泄露-report.md](./report/2026-09-12-timeline持久化死锁与debug桥token泄露-report.md)
- [2026-09-12-会话cwd未传导工具线程grep越界-report.md](./report/2026-09-12-会话cwd未传导工具线程grep越界-report.md)
- [2026-09-12-多会话高频输出热路径串行化与切会话401-report.md](./report/2026-09-12-多会话高频输出热路径串行化与切会话401-report.md)
- [2026-09-12-子代理无法拉起排查-report.md](./report/2026-09-12-子代理无法拉起排查-report.md)
- [2026-09-12-首消息双写与transcript重复渲染-report.md](./report/2026-09-12-首消息双写与transcript重复渲染-report.md)
- [2026-09-13-codex-parity-analysis.md](./report/2026-09-13-codex-parity-analysis.md)
- [2026-09-14-timeline工具块内存放大-report.md](./report/2026-09-14-timeline工具块内存放大-report.md)
- [2026-09-15-进程架构对照-codex与grok-build-report.md](./report/2026-09-15-进程架构对照-codex与grok-build-report.md)
- [2026-09-17-子代理取消后复活根因-report.md](./report/2026-09-17-子代理取消后复活根因-report.md)

### 3.3 handoff（3 份归档 + 1 份建议转 guides）

- [2026-09-12-timeline快照落后自愈-handoff.md](./handoff/2026-09-12-timeline快照落后自愈-handoff.md)
- [2026-09-12-多会话热路径修复进展-handoff.md](./handoff/2026-09-12-多会话热路径修复进展-handoff.md)
- [2026-09-14-main编译回归修复与PR76收口-handoff.md](./handoff/2026-09-14-main编译回归修复与PR76收口-handoff.md)
- [2026-09-13-CNB-NPC全流程开发管线-handoff.md](./handoff/2026-09-13-CNB-NPC全流程开发管线-handoff.md) —— **不是一次性交接，是开发管线手册**，建议转 `docs/guides/` 而非 archive

## 4. 进行中（活文档，勿归档）

| 类型 | 文件 | 状态 |
|---|---|---|
| spec | [2026-09-15-workspace工具层契约重写-spec.md](./spec/2026-09-15-workspace工具层契约重写-spec.md) | 草案待评审 |
| spec | [2026-09-15-前端契约与client-API稳定性-spec.md](./spec/2026-09-15-前端契约与client-API稳定性-spec.md) | G1/G2 已落地，其余待评审 |
| spec | [2026-09-17-工具结果展示层契约-spec.md](./spec/2026-09-17-工具结果展示层契约-spec.md) | 草案待评审；**§10 引用的 TUI 消费面文档在 `qaqh-tui-app` 不存在，属悬空引用** |
| plan | [2026-09-15-workspace工具层契约重写-plan.md](./plan/2026-09-15-workspace工具层契约重写-plan.md) | 草案待评审，P0–P8 **零代码落地** |
| plan | [2026-09-12-session级模型配料-plan.md](./plan/2026-09-12-session级模型配料-plan.md) | 草案待评审 |
| todo | [README.md](./todo/README.md) / [2026-09-17-buglist复核-report.md](./todo/2026-09-17-buglist复核-report.md) / [2026-09-17-buglist复核-checklist.md](./todo/2026-09-17-buglist复核-checklist.md) | 在线 open 项索引；N-5 另有独立 buglist |
| handoff | [2026-09-17-buglist复核九批次执行-handoff.md](./handoff/2026-09-17-buglist复核九批次执行-handoff.md) | 当前交接 |
| report | [2026-09-12-exec与process工具设计评审-report.md](./report/2026-09-12-exec与process工具设计评审-report.md) | 被上述**未落地 plan** 的 §9 与 P6 引用 |
| report | [2026-09-13-codex-exec设计对照与修订-report.md](./report/2026-09-13-codex-exec设计对照与修订-report.md) | 同上 |
| 模板 | [TEMPLATE.md](./report/TEMPLATE.md) | report 写作规范，非内容 |

## 5. 未闭环（留在原位）

### 5.1 有残余项的 buglist（3 份）

| 文件 | 残余 |
|---|---|
| [2026-09-12-exec管道命令间歇性空输出-buglist.md](./buglist/2026-09-12-exec管道命令间歇性空输出-buglist.md) | 7 条待复测，需 Windows 11 + 安装版 daemon + `%TEMP%\qaqh-exec-probe\` 现场 |
| [2026-09-14-timeline工具块内存放大-buglist.md](./buglist/2026-09-14-timeline工具块内存放大-buglist.md) | O-2：TUI 侧非 bash progress 归一/限长未验证（需 TUI 仓访问） |
| [2026-09-16-安全并发与审查登记-buglist.md](./buglist/2026-09-16-安全并发与审查登记-buglist.md) | P2/P3 表剩余行；N-5 已转独立 buglist（`fixed @cef3faa`）；B-5 已修（后端 `eef1231` + TUI `4a795b3`） |

### 5.2 第二轮修复：N-5 + B-5

> **`exec` 在 Level 4 可越出工作区**的原攻击面已按“权限层收口”完成修复。
> 现在 Level 4 仅对 Read/Write 免审批；Exec/Net 统一进入审批，MCP 动态 Exec/Net 同规则，子代理沙箱继续拒绝。
> B-5 已补齐审批信息面：后端 `eef1231` 为 `exec` 生成有界 `action_summary`（含 `command`/`argv`/`args`/`shell`/`cwd`，排除 `env`），TUI `4a795b3` 在弹窗渲染“执行:”行。
> 正式登记：[`buglist/2026-09-17-exec工作区外写入未收口-buglist.md`](./buglist/2026-09-17-exec工作区外写入未收口-buglist.md)。
> 当前状态为 `fixed @cef3faa` + B-5 `fixed @eef1231` / TUI @`4a795b3`；已提交修复分支，待 PR 评审。

### 5.3 其它未闭环

- 性能收益复测（08 的 72k→110k ev/s、09 的 2 MiB→18.9 ms、12 的 62–247 ms、13 的 O(n) 阶跃、09-14 的 63 ms→0.01 ms）：只确认结构性改动在位，**未复跑基准**。
- `2026-09-16-安全并发与审查登记` 的 P2/P3 表：部分行号已失效，需重定位。
- 计划中的工具层契约重写（P0–P8）与 09-17 展示层契约（阶段 1–4）：**均未开工**。

## 6. 归档校验记录

1. **状态回写已完成**：`2026-09-16-apply_patch`、`2026-09-16-edit`、`2026-09-15-热重载`、`2026-09-17-子代理取消后复活` 四份 buglist 已分别回写 `1705449`(#87)、`61b39d0`(#89)、`b4851b0`(#92)、`629637d`(#88)；子代理清单中的 HEAD 不成立项按 `wontfix @33253a5` 关闭。
2. **N-5 已建档并完成修复**：见 §5.2 的 [`2026-09-17-exec工作区外写入未收口-buglist.md`](./buglist/2026-09-17-exec工作区外写入未收口-buglist.md)。当前状态为 `fixed @cef3faa`，全量测试与严格 clippy 已通过，待 PR 评审。

## 7. 归档方式（已执行）

- **已采用方案 A（零风险）**：文件不移动，以本文件作为索引，归档表现为「§3 归档区」的收录状态。
- **方案 B（物理隔离）未执行**：`docs/archive/2026-09/` 目录未创建。当前文档间存在 40+ 处相对链接、30+ 个唯一目标；如后续改为物理隔离，必须在同一个提交内完成 `git mv` + 全量改链 + 链接校验。
