# ADR：统一 Agent 运行时采用冷切换，不保留子代理兼容设施

- 日期：2026-10-26。
- 状态：**Proposed，待仓库所有者批准。**
- 关联规范：[UAR 统一运行时](../spec-unified-agent-runtime.md)。
- 审批范围：AGENTS-x I14 的协议/语义突破，以及旧数据、客户端的切换边界。
- 本文只是设计提案；没有修改生产协议、清理用户数据或批准发布。

## 1. 问题

主/子 agent 虽共用 actor/模型循环，但子代理仍依赖独立 collector、全局 host、按 child session 缓存的 outcome、独立重载和自动关闭。
这些设施既重复业务所有权，又把执行结果绑定到会话而非任务，使静默失败、旧结果误用和旧 collector 关闭新工作成为可能。
具体源码与已运行反例见 UAR §1；不能以现有 helper 单测通过作为端到端可靠性的证明。

增量修补方案 SA-FIX-01 保留旧公开参数/协议和主要结构；本提案不再以此为约束。
目标不是增加第三个协调层，而是让现有子代理执行链退出生产。

## 2. 提议决策

### 2.1 只有一个 agent 运行时

root/child 都是普通 session agent，使用同一个 SessionSpec、AgentHandle、mailbox、writer、execution 状态机、supervisor 和恢复入口。
parent/role/工具范围/审批能力是数据和策略，不产生第二套 worker 构造和终态逻辑。
每次 Trigger 以 canonical input_id 标识 execution；agent outcome、wait、取消、结果交付和去重都绑定该次 execution。

删除 collector/HostTransport、SubagentHost 生命周期接口、全局 host 安装槽、子代理专用 spawn/reload、按 child ID 的结果缓存和终态去重。
18 个现有工具保留业务能力，迁移到实例注入的 typed 应用端口后删除 `qaqh-subagent` crate，不留空壳、别名或 re-export。

### 2.2 明确改变的产品和 API 语义

| 当前形态 | 冷切换后 |
| --- | --- |
| `subagent.spawn` 服务 / ActionRequest 只造 worker | 删除；统一 create/delegate 用例必须有 parent scope、持久意图和 execution receipt |
| AgentOutcome 的 answer + exit_code | 删除 agent exit_code；返回带 execution_id 的结构化 terminal/error/answer/source fact |
| 每个 child 默认 120s 最大寿命 | child 不再有特殊寿命；execution 总 deadline 默认不设，显式预算从实际 started 计时 |
| 终态后 collector 自动 SessionClose | 完成与卸载分离，统一 idle-unload；close 是显式树生命周期操作 |
| steer/interject idle 时留到未来 | 准入时固定 execution；idle/已结束明确 target_not_running |
| wait 读 child 历史 outcome | 捕获每 target 的 execution，wait 超时不取消目标，读取错误不返回空成功 |
| 子代理 model/base_url/api_key 部分覆盖 | 删除裸端点/密钥半覆盖，使用具名完整 binding 与安全 credential 引用 |
| direct child Ringing 输入与 host 等多入口 | 所有入口走同一授权与 typed application 命令，没有 bypass registry 路 |

`spawn_subagent` 等模型工具名字可以保留以减少无意义重命名，但 schema/输出/提示描述按新语义生成；名字保留不表示兼容旧实现或旧客户端。
超时范围、配置形状与工具 All/Only 策略以 UAR §8～§10 为准，不对旧参数 silent clamp 或隐式映射。

### 2.3 wire 与消费者

不在 Ringing major 2 中暗改语义。破坏性协作 API、执行状态、close/cancel/receipt 与正文投影随已规划 major 3 的应用/SDK 消费契约同批锁定。
不另外造一套 UAR HTTP/SSE 协议；认证、连接身份、稳定命令去重和本地应用入口继续按协议专题治理，不恢复已移除的 driver 席位。

本切片只有一个生产业务所有者、一个在线 schema 和一个执行链。**不接受在线 v2→UAR facade、双跑 loop、双写会话或失败自动降级。**
部署切换是 coordinated upgrade：发布前列出实际支持客户端/SDK 的精确版本与提交，逐一升级；不支持的组合返回 `upgrade_required` 或协议版本不匹配，不假装受理成功。
若不能协调全部当前支持客户端，本切片不得提前生产切换；不能据此重新引入无限期兼容桥。

## 3. 持久数据不清库，迁移只在离线边界

1. 停新准入、停止/收割执行，备份旧数据根；不能边运行旧任务边起新 writer。
2. 独立 importer 以只读方式解析旧 facts/messages/bundles，将可证明的状态导入 staging 当前格式。
3. 缺执行关联、blob、provider 私有 bundle 或副作用证据时，不补成功、started 或可 resume 承诺；显式报告 interrupted/indeterminate/仅历史可读。
4. 引用完整性、上下文保真、图/队列/终态一致性和客户端验证通过后才切 active 根与格式标识。
5. 在线代码只解析当前格式；旧 decoder 只存在于离线 importer，不成为 read fallback。
6. 原根和备份不自动删除；新格式一旦写入，旧二进制禁止打开它。回退必须保留新写入数据，不能无提示恢复旧备份覆盖新消息。

禁止以“彻底重构”为理由静默删除历史。若用户选择完全不导入某批历史，必须在实施时获得另一次明确数据处置授权；本 ADR 不含该授权。

## 4. 备选方案与拒绝理由

### A. 在现有 collector 上补 execution_id、错误与重试

可缓解局部症状，但保留观察流驱动业务终态、两套停止/恢复和会话级全局槽。
这不满足统一所有者与旧链删除目标，拒绝作为 UAR 交付。

### B. 新 runtime 通过 adapter 长期兼容旧 host/服务/schema

能较早复用旧消费者，但让旧概念、兼容测试、配置分支和数据双读长期支配新核心。
故障时容易回到旧路径，无法证明唯一 writer/结果/停止所有权，拒绝。

### C. 删除全部数据、只支持空根

实现最简单，但把数据损失当架构简化，违背事实与正文存续承诺；没有用户独立数据授权，拒绝。

### D. 本提案：协调升级 + 离线导入 + 在线旧链删除

成本更集中、切换门槛更高，但运行时只有一个模型，故障、恢复与客户端行为能整体证明。选择此方案。

## 5. 影响与边界

- 本次不是低风险补丁：需要共享 agent loop、提交/context、工具执行回投、客户端投影和迁移的完整切片。
- 无法在现行客户端不变时无声投放；必须有冷切换窗口、验证根和回退计划。
- 业务终态与物理停止分离；不响应取消的 Rust 线程不能强杀，残留任务仍被监督、占配额并阻止冲突复用。
- 不承诺 OS/远端 exactly-once；承诺会话提交/准入/结果应用的幂等，以及副作用未知时诚实对账。
- 保留文件安全、权限、账号、多模态与 provider bundle 保真、18 工具业务能力；不要求顺便实现 ACP、市场或重写有效文件算法。
- UAR 与现有 agentloop 规范共用一个实现/任务所有者，禁止各建一个 supervisor 或 loop。

## 6. 审批与验收

所有者需确认：

1. 同意移除列出的旧 API/DTO/全局 host/crate，允许不兼容工具输出和配置形状。
2. 同意随 major 3 协调客户端切换，不要求长期在线兼容旧消费者。
3. 同意默认无 agent 总 deadline、显式预算、结构化终态和无每轮自动 close 的语义。
4. 同意离线导入、保留旧根、未获足够证据不宣告可恢复的规则。

批准应以本 ADR 状态和审批记录体现，不能由实现者自行把 Proposed 改成 Accepted。
实际完成仍须满足 UAR 的删除清单、A01～A27、跨仓客户端和迁移闸门；ADR 被批准本身不是实现完成证据。
