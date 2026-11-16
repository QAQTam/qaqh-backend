# ADR-0026: exec 订阅终态推送（write_stdin notify_on_exit）——决策 8 的订阅制定向回调

状态: `已采纳` · 决策日期 2026-11-14 · 决策人 仓库所有者 · 实施: 切片 1 已落（本文档同批）

## 背景

ADR-0017 决策 8 移除了 exec 终态唤醒注入链（`route_exec_exit` / `<qaqh_exec_session_exit>`），
理由如实登记：原机制在后台 exec 终态且模型未读时**无条件**注入（含 output tail），造成三类
实害——

1. **竞态双交付**：模型恰好在生命周期结束时用 write_stdin 读到 stdout/err，终态结果既从
   poll 返回、又从注入链到达 → "莫名其妙的注入"；
2. **重复塞输出**：注入把 output tail 复制给模型，与 write_stdin 的读取语义重复 →
   模型抱怨"都 write_stdin 了，结果结束又注入"；
3. **后台服务器误唤醒**：vite 等常驻服务 + timeout_ms 强退 → 终态 → 照样注入。

决策 8 的行为代价："后台 exec 完成不再唤醒空闲模型——模型在下次交互/poll 时经
write_stdin 读到终态"。这个缺口在长构建场景真实存在：模型要么原地轮询烧回合，要么结束
回合后永远等不到结果（goal 续跑仅覆盖有 Active goal 的会话）。

本 ADR 恢复**订阅制**推送：修的是决策 8 诊断的根因（无条件推送），不是原样回退。

## 决策

1. **模型面字段**：`write_stdin` 新增可选参数 `notify_on_exit: bool`（仅 `chars` 为空时
   生效；非空 chars 携带订阅 = 工具面确定性报错）。语义是**终态通知所有权转移**，不是
   "转后台"（会话本就是后台语义）：armed 后模型应结束回合，harness 在进程退出时以
   canonical system 消息推送终态，恰一次。
2. **原子订阅，竞态归零**（灭害 1）：订阅判定在 manager `poll` 内与会话 core 锁同一临界区
   完成——终态已定 → 不订阅，本次调用按普通终态读交付（推送不会发生）；仍在跑 → 置
   `notify_armed` 后**立即返回**（不等 yield 窗口、不消费 model_pending，待读缓冲留给
   终态后一次性收割）。finalize 在同一把锁内置终态，"先终态还是先订阅"是全序的，
   恰一次交付由构造保证，不靠去重补丁。
3. **恰一次交付**：`notify_armed` + `notify_delivered` 双标志在会话 core 内；任何通道
   （poll 当场交付或端口推送）送达后回写 delivered，另一通道跳过。订阅后模型继续 poll
   合法（普通读），不产生第二次推送。
4. **注入物不带输出正文**（灭害 2）：推送 = 小结构事实（command、exit_code、终态类别）+
   按需读取指引（终态后一次 write_stdin 收割待读缓冲；全文 transcript 在 content store /
   append-only 日志，ADR-0017 决策 2/切片 1 已就位）。特征标记
   `<qaqh_exec_session_terminal>`（与 `GOAL_CONTINUATION_MARKER` 同一审计先例，据此识别
   自动续推轮）。
5. **可推送终态白名单**：仅 `Exited`（含非零）与 `TimedOut`。`Cancelled`/`Interrupted`
   不推送——都是有主体的主动停止（用户 Ctrl-C/terminate、daemon 关停、回合取消连带），
   回推只会制造噪声（灭害 3 的语义对应物：订阅只回答"进程自己结束了"）。
6. **推送路径全部复用既有机构**：端口回调（manager `finalize` 在 core 锁外触发 → registry
   锁短临界区，`route_turn_terminal` 装配方式同构）；投递目标解析 handoff 链头 +
   `session_handed_off` 封箱闸（`deliver_continuation` 同款）；冷会话
   `ensure_loaded_for_command`；忙会话回合边界合流（`send_input` 入队语义）；投递失败由
   daemon 周期扫描补投（`sweep_active_goals` 同款，新增 `sweep_exec_notices`）。
7. **armed 状态为内存态**：daemon 重启杀全部子进程，无跨重启义务；会话显式 close 仍走
   `shutdown_session` 杀进程组（armed 随之湮灭，无需清理）。goal 交互零冲突：armed 后
   结束回合，goal sweep 照常续跑；推送到达时忙则合流。`goal_continuation.md` 的
   verified wait 措辞扩入 "armed 订阅 + 结束回合等推送" 也算 verified wait。
8. **模型面指引**：订阅成功的工具结果在 output 尾部附加行为契约文本（结束回合、勿再轮询、
   终态后一次收割的读取方式）；`write_stdin` meta 描述同步更新。`notify_armed` 不进
   模型面 JSON。

## 被否选项

- **原样恢复决策 8 链路（无条件推送）**：否。三类实害是决策 8 的诊断结论，回退即复发。
- **`background: true` 命名**：否。会话本就是后台语义，字段描述的是已经发生的事；
  `notify_on_exit` 自描述"通知所有权转移"，且不给 silent-detach 留第二档位——不订阅
  （决策 8 后的默认行为）就是 silent detach，加档位是冗余状态。
- **armed 状态持久化**：否。daemon 重启杀全部子进程，跨重启无通知对象；进程内 Mutex
  标志已满足"恰一次"。
- **推送携带 output tail**：否。决策 8 点名的"重复塞输出"；正文一律按需读取。
- **`exec_command` 起始订阅（一次往返完成启动+订阅）**：延后至切片 2。同一 armed 标志
  语义，实现近零成本，但不阻塞本切片。

## 落地清单

- [x] manager：`ExecTerminalNotice`/`ExecNotifyPort`/`set_notify_port`；
      `SessionCore.notify_armed/notify_delivered`；poll 订阅原子判定；finalize 推送门
      （core 锁外触发端口）；`mark_notify_delivered`；`armed_undelivered`
      （`crates/qaqh-process-tools/src/exec_session/manager.rs`）。
- [x] 工具面：`WriteStdinArgs.notify_on_exit` + 空 chars 校验 + 订阅指引文本 + meta 描述
      （`crates/qaqh-process-tools/src/exec_session/tool.rs`）。
- [x] 请求/结果桥：`WriteStdinRequest.notify_on_exit`、`ExecCommandResult.notify_armed`
      （`crates/qaqh-tool-core/src/tool_api/context.rs`）；agent-runtime 透传
      （`crates/qaqh-agent-runtime/src/tool_job.rs`、`lib.rs` re-export）。
- [x] registry：`route_exec_terminal`（链头解析 + 装载 + canonical send_input + delivered
      回写）与 `sweep_armed_exec_sessions`（`crates/qaqh-runtime/src/registry/exec_notify.rs`，
      模块注册于 `registry/mod.rs`）。
- [x] 装配：service init 注入端口 + `sweep_exec_notices`（`crates/qaqh-runtime/src/service.rs`）；
      daemon 周期扫描挂点（`crates/qaqh-daemon/src/server.rs`）。
- [x] 模板：`goal_continuation.md` verified wait 措辞扩入订阅。
- [x] 测试：订阅即返回+恰一次推送、终态后订阅当场交付不推送、Cancelled 不推送
      （`manager.rs` 测试模块；模块为 `cfg(all(test, unix))`，Windows 开发机仅人工复核，
      CI/linux 侧执行）。
- [x] 追加（切片 2 推演时发现的契约缺口）：读取通道交付终态亦置 delivered——
      `result_from_core` 对 armed+terminal 会话置位（armed 后模型主动 poll 捕获退出、
      启动调用窗口内退出，均抑制推送）；finalize 在 publisher.finish 后、端口触发前
      复核门，闭合"等待中的 poll 恰好捕获退出"的常见交错；残余微秒级交错后果为
      无正文重复通知，语义幂等。测试：`terminal_poll_delivery_marks_armed_session_delivered`
      （无端口场景经 armed_undelivered 收敛确定性断言）。
- [x] 切片 2：`exec_command(notify_on_exit=true)` 起始订阅——`ExecSessionStart.notify_on_exit`
      + `start_platform` 窗口前原子置 armed；窗口耗尽仍存活 → yielded + notify_armed
      （工具面附加同一指引文本）；窗口内退出 → 启动调用直接交付终态（读取通道置
      delivered，推送让位）。schema 字段描述承载模型调用指引（何时该用/勿轮询/
      常驻服务勿用/timeout_ms 交互），`write_stdin` 字段描述同步补“新命令优先
      exec_command 起始订阅”的交叉引用。测试：窗口内退出不推送 + 窗口耗尽 armed
      + 恰一次推送（`manager.rs` 测试模块）。
- [ ] 契约影响：模型面 tool args schema（schemars 运行时派生，无 golden 冻结）新增可选
      字段；ringing contract 无变化，无 refreeze，ADR-0010 账本登记"无契约产物变化"。
