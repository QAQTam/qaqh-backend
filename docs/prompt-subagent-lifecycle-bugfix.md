# 直接交给实现模型的任务指令

你在 `qaqh-backend/` 修复子代理系统，任务 ID 为 **SA-FIX-01**。

**现在直接实施，不要只回计划，不要先问从哪一项开始。** 先读 `AGENTS-x.md`，然后完整读 [修复 spec](spec-subagent-lifecycle-bugfix.md)。用户要求最多三轮结束：本轮就是完整实施轮，后两轮只供我们审阅后修缺陷，不是让你把功能分三次交付。

## 已作出的决定，不要反复探索或重新设计

1. canonical log + per-session blob 为权威，不用 Ringing TTL cache 存最终答案。
2. actor 身份长期存在；每次 Trigger 的唯一 `message_id` 就是 `execution_id`，关联 child 的 input/turn。collector、结果、wait、parent completion 都绑定本次 execution，不能只按 session id。
3. 同 agent 串行执行 followup；保留历史上下文与父会话有效配置/权限。旧 collector 不能关闭新任务，旧结果不能让新 wait 提前成功。
4. Queue/Steer/Interject 在工具 lap 安全点进入下一次 provider request；idle 不启动新回合。忙碌的 Trigger 排队，不当纠正消息注入。
5. path/id/相对路径统一解析，wait 返回自己开始等待时固定的执行快照，不能重新按原始字符串猜目标。
6. 重启真实恢复 catalog/结果/未收尾 execution；不自动重跑不可证明幂等的任务。关闭与订阅回收不持 registry 锁做 IO/join，不留下 detached 轮询线程。
7. 18 个工具保留；不靠删功能、改描述、扩权、改绿测试或新增长期 fallback“修复”。task/board 沿用既有状态机，只补 spec 明确列出的契约缺口。

## 先看这些接缝，不要重新通读整个仓库

- `crates/qaqh-runtime/src/agent/turn_lap/{parse,gate,backfill}.rs`
- `crates/qaqh-runtime/src/agent/{tool_runtime,loop_injection,loop_dispatch_conversation,spawn}.rs`
- `crates/qaqh-runtime/src/{host_impl,registry,agent_graph,subagent_recovery,subagent_supervisor}.rs`
- `crates/qaqh-session/src/{canonical/tool_ledger.rs,canonical/blobs.rs,session_fact_v2/,projection/}`
- `crates/qaqh-subagent/src/{lib,host,task_tools,board_tools}.rs`
- `crates/qaqh-runtime/tests/{host_direct,subagent_inprocess,tool_surface_schema_shape,tool_ordering_contract}.rs`

spec 已给出根因、行为裁决、工作包 A～H 和验收矩阵 T01～T20。按它施工，不需要等我逐条授权。

## 本轮工作方式

- 不使用子代理。允许隔离测试启动必要的确定性 actor；不得连接真实模型服务或使用真实用户数据/密钥。
- 首先做四个行为红测试：真实生产路径答案回传、busy 安全点注入、多次 followup、service 重建恢复。编译失败不算红测试；mock 答案下载/手工注入结果不能代替这些测试。
- 按 spec 第 5 节顺序实现全部 MUST，自行运行→看失败→修复，直到相关测试通过再一次性交付。
- 不重写整个架构、不引入新依赖；大文件的本次职责抽到小模块，不能继续膨胀超长文件。新增跨 crate 公共错误要有类型，会话状态显式传参。
- `AGENTS-x.md` 引用的两份架构文档在审计基线缺失；spec 已补本次必要约束，不要反复寻找，也不要伪造你读过。若工作区现在已有，先读再做，不覆盖。
- 依赖缺失/DNS 失败精确记录，不循环盲重试；先继续能离线运行的验证。不能把未跑过的测试写成通过，不能以 `cargo check` 代替闭环验收。
- 本轮不要提交“先修了 A，其余以后”；spec 内的问题不能转交下一轮。若确有硬阻塞，报告具体条款、证据和完成状态，不谎报已完成。

## 交付

1. 完成代码和回归测试，保持公开工具参数和 Ringing 路由兼容；必要的持久化关联字段只能 additive/default。
2. 写 `docs/report-subagent-lifecycle-bugfix.md`，对照 A～H、T01～T20 列准确测试名/结果/未验证原因，附四个端到端闸口的红→绿证据和所有命令退出码。
3. 最终回复简洁列出：完成/未完成、关键实现符号、验证结果、剩余阻塞；不要只说“单测通过”。
4. 不修改 spec 的验收条款，不创建新路线图把本轮必做项挪走，不替审阅者批准已知缺陷。

开始实施。
