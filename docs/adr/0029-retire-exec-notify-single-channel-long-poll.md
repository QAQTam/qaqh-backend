# ADR-0029: 砍掉 exec 订阅终态推送，回归单通道长轮询（取代 ADR-0026）

状态: `已采纳` · 决策日期 2026-11-18 · 决策人 仓库所有者（明确指示"学习 codex 重新砍掉"）· 实施: 本批同落

## 背景

ADR-0017 决策 8 移除无条件 exec 终态注入链（三类实害：竞态双交付、重复塞输出、
后台服务器误唤醒）。ADR-0026 以订阅制恢复推送，声称"原子订阅，竞态归零""恰一次
交付由构造保证"。本次对注入链路的静态审计（manager.rs / registry/exec_notify.rs /
session_loop.rs 三方锁序与调用链推演）证明该声称不成立，根因有二：

1. **入队 ≠ 送达，delivered 标志落位过早**。`route_exec_terminal` 在
   `send_input` 返回 Ok 后立即 `mark_notify_delivered`；但忙会话里 Ok 只意味着
   进了 `inner.queue`，真实消费在回合边界（`finish_active` 弹队首开新回合）。
   时序：finalize 复核门通过 → 端口回调（DB 读 + 链头解析 + 冷装载）→
   `send_input` 入队 → 模型在**同一活跃回合内** `write_stdin` 读到终态与全部
   输出（读取通道亦置 delivered）→ 回合结束，排队通知仍专开一个新回合，把
   模型早已处理完的过期事实再注入一次，并指示它收割已空的缓冲。ADR-0026
   登记的"残余微秒级交错"诊断错误：复核门只覆盖 `publisher.finish` 期间的
   poll；通知一旦入队，本回合剩余任意时刻（可达数分钟）的终态读取都无法
   撤销它。这正是用户报告的"模型意外收到早就通过 write_stdin 的消息"。
2. **反向漏洞与死路**：`abandon()` 清空队列时 delivered 已置 true → 通知永久
   丢失、sweep 永不补投（同一根因的另一面）；handoff 链头投递不转移 exec
   所有权，继任会话收到通知却 `write_stdin` WrongOwner，指令不可执行。

codex（D:\Projects\codex，unified_exec）对照：**模型侧没有第二条交付通道**。
`ExecCommandEnd`/`ExecCommandOutputDelta` 是 transient 非持久事件
（rollout/src/policy.rs 明确不持久化），只进 UI；终态只经读取通道
（exec_command 初始窗口 / write_stdin 空 chars 长轮询）；空轮询钳制
[5000, background_terminal_max_timeout]，默认上限 300000ms 可配；非空写入
≤30000ms。恰一次由单通道构造平凡成立，无需任何跨通道去重标志。

## 决策

1. **整体砍掉 ADR-0026 推送机构**，不留兼容层（宪法 4）：
   `notify_on_exit` 字段（exec_command 与 write_stdin 两工具）、
   `ExecTerminalNotice`/`ExecNotifyPort`/`set_notify_port`/
   `mark_notify_delivered`/`armed_undelivered`、`SessionCore` 双标志、
   finalize 推送门与复核门、`registry/exec_notify.rs` 模块、service 端口
   装配、daemon `sweep_exec_notices`、订阅指引文本（append_subscribe_notice）。
   两工具 args 均 `deny_unknown_fields`，旧字段再传入 = 确定性报错。
2. **yield 双轨制（codex 默认值同款）**：空 chars 读 = 长轮询通道，
   5000..=300000ms，缺省 5000；非空写入 250..=30000ms，缺省 250。
   `write_stdin` meta default_timeout 30s→310s（诚实元数据：该工具可合法
   阻塞至 300s；此超时在 exec_session 路径不被强制消费，仅元数据）。
3. **长轮询是唯一等待通道**：进程退出时 `wait_for_call` 终态分支立即返回
   （100ms condvar 切片），交付终态 + 窗口内捕获的输出。模型指引落在工具
   meta 描述、qaqh-exec skill、goal_continuation 的 verified wait 措辞。
4. **权限**：空 chars `write_stdin` 恒 Read（订阅特例随机构删除）；非空恒 Exec。
5. **行为代价（如实登记）**：模型结束回合后，后台 exec 完成不再唤醒空闲
   模型——下次交互/poll 时经读取通道读到终态（ADR-0017 决策 8 的原始代价）。
   300s 上限覆盖多数长构建：单次工具调用阻塞等待，不烧模型回合；超 300s
   需续 poll（同一回合内的多次工具往返，不是多个模型回合）。有 Active goal
   的会话由 goal 续跑兜底。
6. **可配置性延后**：codex 的 `background_terminal_max_timeout` 是配置项；
   本切片用常量 300000（= codex 默认）。日后如需定制，按
   `hooks::set_exec_max_output_tokens` 既有门面注入模式接线；本切片不留
   无人安装的死钩子（宪法 5）。

## 被否选项

- **保留推送现状 / 原样恢复**：否。根因是结构性的（入队≠消费），ADR-0017
  决策 8 的三类实害以订阅制新形态复发，用户实测抱怨在案。
- **消费时裁决恰一次（保留推送的最小正确修复）**：通知入队不置 delivered，
  `finish_active` 弹出时向 manager 复核、读取通道已交付则丢弃不开回合；
  同时修 handoff 所有权移交。此方案能保住推送价值，但需要 loop 队列与
  manager 的跨层协调（队列输入携带 exec_session_id + 复核回调），协调面
  正是本次事故根源。所有者裁定学习 codex 单通道，彻底消灭协调面。**防重提
  注记：日后若恢复推送，必须建立在消费时裁决 + 所有权移交之上，不得复用
  入队时裁决。**
- **`notify_on_exit` 保留为 no-op 兼容字段**：否（宪法 4，兼容套层即懒政）。
- **只扩 yield 上限、保留推送**：否。根因与窗口长度无关，扩窗不治病。
- **300s 上限做成配置项**：延后而非否决（决策 6）。

## 落地清单

- [x] manager：删 `ExecTerminalNotice`/`ExecNotifyPort`/`notify_port`/
      `set_notify_port`/`mark_notify_delivered`/`armed_undelivered`、
      `SessionCore.notify_armed/notify_delivered`、start 起始订阅、poll
      订阅分支、`result_from_core` delivered 置位、finalize 推送门/复核门；
      删 6 个 notify 测试与 `poll_request`/`yield_session_id` helper
      （`crates/qaqh-process-tools/src/exec_session/manager.rs`）。
- [x] 工具面：两工具 `notify_on_exit` 字段删、`append_subscribe_notice` 删、
      yield 双轨校验（空 5000..=300000 / 非空 250..=30000）、meta 描述更新、
      write_stdin default_timeout 310s（`exec_session/tool.rs`）；
      `exec_session/mod.rs` re-export 收窄。
- [x] 桥：`ExecCommandRequest`/`WriteStdinRequest.notify_on_exit`、
      `ExecCommandResult.notify_armed` 删（`qaqh-tool-core/src/tool_api/context.rs`）；
      agent-runtime 透传删（`tool_job.rs`）、re-export 收窄（`lib.rs`）。
- [x] 权限：`write_stdin` 订阅特例删（`qaqh-permission/src/permission.rs`）。
- [x] registry：`exec_notify.rs` 删除、`registry/mod.rs` mod 声明删；
      service：`notify_service` + `set_notify_port` 装配块、
      `sweep_exec_notices` 删（`qaqh-runtime/src/service.rs`）；
      daemon：60s sweep 块删（`qaqh-daemon/src/server.rs`）。
- [x] 模板：`goal_continuation.md` verified wait 改长轮询措辞；
      `qaqh-skills/assets/bundled/qaqh-exec/SKILL.md` 订阅条目改长轮询指引。
- [x] ADR-0026 状态改"被取代"，README 索引同步。
- [x] 验证（2026-11-18，Windows 开发机，debug profile）：
      `cargo check -p qaqh-process-tools -p qaqh-tool-core -p qaqh-permission
      -p qaqh-agent-runtime -p qaqh-runtime -p qaqh-daemon` 全绿（41s）。
      `cargo test` 同七 crate + qaqh-workspace（`--no-fail-fast`）：改动 crate
      自身测试全绿——agent-runtime 53、daemon 56、permission 38、runtime lib 59
      + 集成 12、workspace 集成 45、process-tools 84。全部 10 个失败经
      **HEAD 基线 worktree 同特征集复跑逐一实证为既有**（`native-tool-plan`
      特征由 runtime 统一开启，单 crate 跑会漏测试，基线须同选包）：
      process-tools 沙箱逃逸/OEM 解码 ×2（环境性）、workspace
      authorization×3 + exec_progress + schema_spot_check、audit_fail_closed、
      audit_ledger、tool_sdk_parity（20≠22，退役工具未更新期望）、
      runtime/workspace 的 oneOf schema 闸门（`ShellName` enum 既有生成形态）、
      tool-core canonical_patch_views。本次改动零回归。
      manager 的 unix 测试模块（含删除的 6 个 notify 测试）Windows 不编译，
      CI/linux 侧执行。
- [x] exporter 契约检查（`cargo run -p qaqh-contract-export -- check
      --against docs/api-surface/v4.0`，debug profile）：本工作区实测
      **仅 2 个产物字节差异**——`fixtures/pair-result.json` 与
      `fixtures/service-daemon-version-result.json`，内容均为
      `daemon_version "4.0.1"→"4.0.2"`：v4.0 金标准在 4.0.1 时重切
      （ADR-0027），HEAD release 批次 bump 版本后未重导这两个带版本号的
      fixture，属 **HEAD 既有版本漂移**，与本批无关（本批不触任何 wire
      DTO；版本来自 workspace Cargo version，本批未动）。是否机器重导由
      所有者按 ADR-0027 裁定，不擅自代劳（宪法 16）。
      **基线 worktree 对照对此闸门无效**：fresh checkout 经 eol 转换把
      冻结产物 LF→CRLF，exporter 按字节比产生 629 个假差异（两树冻结面
      逐字相同、仅行尾不同，已 diff 实证）；worktree 基线法只适用于
      测试闸门，不适用于字节级契约闸门。宪法 20 引用的
      `check-lts-api-surface.py` / `check-surface-change-policy.py` 已随
      4.0.0 闸门拆除（ADR-0028 考据在案），exporter check 为现存唯一官方闸门。

## 契约影响

模型面 tool args schema（schemars 运行时派生，无 golden 冻结）：删两个可选
字段 + 描述变更；ringing contract 无变化。分类以官方检查器实算为准（宪法 15）。

## 过程限制（如实登记）

- 规则 19 的 `git fetch` 与 `origin/LTS-v3.0` 比对**未能完成**：本环境到
  github.com 的连接被重置/超时，本地亦无该远端引用。合并前须补做比对并维持
  main ⊇ LTS-v3.0 超集不变量。
- 编辑期间发现 manager.rs 实际内容与首次读取存在漂移（`set_notify_port`
  闭包变量名 `|error|` vs `|e|`），疑似并发写入者；以编辑工具的当前内容
  回报为准逐处核对。合并时按规则 19 留痕。
- 竞态时序原为代码静态推演，**本批实施期间已动态复现**（旧 daemon 二进制）：
  `cargo check` 会话（execs_01M7Q9ZSW2EKZR48Y32GFR1X9B）退出后推送入队，
  模型在活跃回合内先经 write_stdin 收割了完整输出与终态（返回
  outcome=completed），数分钟后排队的 `<qaqh_exec_session_terminal>` 仍
  专开新回合注入过期事实——残余窗口实测横跨整个活跃回合（本例为数分钟
  的 ADR 撰写时间），直接证伪 ADR-0026 的"微秒级交错"诊断。
  manager 测试模块 `cfg(all(test, unix))`，Windows 开发机仅人工复核，
  CI/linux 侧执行。

## 附录：注入语法的备查取证（不复活推送）

决策 5 之后，所有者另询"迟到终态能否以 toolcall-toolresult 对注入"。经
`127.0.0.1:8787/v1`（mutil-ai 网关）实测（hy4flash-free；auto 路由上游
额度耗尽 429）：

- 完整 fake 对（伪造 assistant `tool_calls` + 配对 tool result，全新唯一
  id，**无需 tools 数组**）→ 200，模型 reasoning 原样消费伪造输出。
- 孤立 toolresult / 悬空 toolcall / 同 id 双 result → 该路由均 200（宽松
  透传），但 mutil-ai 库默认策略是 orphan **Drop**（静默丢内容）、
  missing **Synthesize**、重复 id 拒绝——严格上游会 400 或被修复策略
  吃掉。**完整相邻对 + 唯一 id 是跨 provider 最小安全单元**。
- 适配不在 harness 逐 provider 做：qaqh-gate 的协议适配已整体移交
  mutil-ai（normalize 按 Protocol 转方言），harness 只发规范对。
- 诚实性：fake assistant call 声称模型发过它没发过的调用；用于迟到终态
  时最自洽的虚构是"harness 代 poll 了一次 write_stdin"，timeline 落库
  须带 provenance 标记（actor 机制已有位置）。

本附录仅备查：exec 终态交付维持决策 3 的单通道长轮询；若日后按被否选项
注记恢复推送或用于其它注入场景，规范工具对是首选注入语法。