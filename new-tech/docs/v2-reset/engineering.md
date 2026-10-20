# v2.1 → v3.0 实现简化与测试取舍规则

状态：新实现的工程约束；本文不表示 Rust、防御分支或测试已清理。
下列旧结构与规模取样于 2026-10-10 的 `v2.0.0-beta4`，不是当前产品完整审计或删除名单。
2026-10-20 校准：当前源码起点 `37167d7`，按 [rollout](rollout.md) 随功能发布；先刷新 [preservation](preservation.md)，不能只保留 beta4 测试。
架构所有权以 [重构入口](README.md) 和 [依赖矩阵](dependencies.json) 为准。
消息/恢复见 [messages](messages.md) / [agent](agent.md)，远端执行见 [client-tools](client-tools.md)。
不新增 bug 账本；实施时直接更新当前责任和退出结果，历史机制由冻结 tag 保留。

## 1. 已核实的旧结构补偿

| 当前做法与证据 | 保护的旧前提 | 新线处理 |
|---|---|---|
| [manager 读取重复 msgid 时 keep-first](../../../crates/qaqh-session/src/manager.rs) | 双写或异常重复，读取自动丢弃重复项 | 正常读取不吞同身份异内容冲突；旧数据修复归离线 importer，新提交端保证身份与幂等 |
| [图片后补同步 pending 克隆，否则 SaveFull](../../../crates/qaqh-message/src/store.rs) | 消息与附件分阶段修改，多份状态不同步 | 输入与附件在明确提交边界收口，后续变更是显式命令；退出全量重写补偿 |
| [恢复缺工具结果时补“未执行”](../../../crates/qaqh-message/src/store.rs) | 消息配对不足以确认执行结论 | 依据 intent/journal/receipt 对账，缺证据为 Indeterminate，不补造副作用结论 |
| [panic 后 reset engines/清 cancel/继续接命令](../../../crates/qaqh-runtime/src/agent/loop_core.rs) | 共享 engine 状态跨模块恢复 | supervisor 隔离失败范围；恢复依据 committed facts 与收割证据，不假设 reset 即一致 |
| [workspace 缺失回落 cwd、manager 缺失给空工具集](../../../crates/qaqh-workspace/src/runtime.rs) | 全局状态和隐式装配 | 构造注入 WorkspaceHandle/能力来源；必要资源缺失给具名错误 |
| [必要 canonical ledger 可缺失并返回 Ok](../../../crates/qaqh-runtime/src/agent/loop_dispatch_conversation.rs) | 持久/临时/兼容运行路径混合 | 持久 actor 构造时保证 commit port 存在；临时任务显式区分，不在运行期跳过必要事实 |

这些位置是在确认结构问题，不能据此先删 guard 而继续运行旧前提。
`catch_unwind`、默认值、clamp、锁中毒处理或去重不是单凭语法即可判定的坏代码。
每项补偿退出时，必须说明原前提如何消失，或失败交给了哪个明确所有者。

## 2. 实现规则

### 2.1 内部状态由类型和所有权约束

- 必要 store/workspace/executor 在构造完成时存在，不用 Option + skip 表示“可能没装配好”。
- 持久会话、临时任务、未连接客户端、失去执行证据是不同显式状态；不以空 ID/空路径代替。
- 用 enum 与受限转换表达 opened/prepared/started/finished，不用多个 bool 组合猜状态。
- 分清原始输入、ValidatedCommand、资源句柄、CommittedIntent 与执行结果；类型不能由任意调用方伪造授权。
- 使用借用、所有权和具名任务句柄收口生命周期；不复制多份 pending/消息/取消权威再写同步补丁。
- 同一种不变量由实际所有者保证；各层不反复解析内部 JSON 或验证已证明的静态前提。

例如 keep 必须非零时，可在输入边界构造非零值；不能在内部偷偷 keep.max(1) 改变请求含义。
如果 clamp 本来就是公开的预算上限政策，应明确它的含义与最终采用值，可以保留。
类型设计只限制静态前提；凭据吊销、文件版本、连接代际等可变化条件仍须运行时复核。

### 2.2 校验按边界归属

工具模型参数的已知拼写容错属于必要输入边界，不属于可随意删除的内部补偿：保留 schema 归一化与 typed lenient 的双层保护，
覆盖 union/type-array 的实际形态及字符串整数等已接受输入。容错不得放宽 API 授权、未知字段、范围或副作用准入。
同样，provider 流中的 tool-call 身份/参数增量归一化与无法执行调用的可见失败必须迁移，不能把丢调用包装成“更薄”。

当前还接受 file_path/path/file、edit 的 old_string/old_str 与 new_string/new_str，以及单字符串提升为单元素字符串数组。
别名必须在 typed 解析、授权资源提取、同文件冲突排序、审计和展示中同义，不能只加 serde alias。
同文件双 edit 或混用新旧键仍须串行，独立文件保留并行；路径、schema 与权限范围不能因容错扩大。
接收端别名不必全部暴露到模型 schema；模型清单按固定快照保持确定性，不通过随意改 schema“修容错”。
未知/冲突字段仍按边界契约拒绝，不把 `"a,b"` 猜成两个路径。
配置不靠 apply_profile/reload/child 三份手工字段清单维持一致；完整绑定和 typed snapshot 由 resolver 一次构造。
当前 credential 缺拷贝、命名 secret 与 Main 继承、CAS 缺失的反例及全链路验收见 [configuration](configuration.md)。
进程内单写锁与“只发脏字段”都不能证明多资源/多端编辑没有过期覆盖。

| 边界 | 必须保留 |
|---|---|
| API/SDK/native IPC/客户端工具输入 | 形状、版本、大小、身份、scope、资源引用与有界队列 |
| provider/MCP/LSP/ACP | 协议、内容布局、关联 ID、能力协商与不可信结果校验 |
| 磁盘/导入 | schema、完整性、提交边界、内容存在、冲突与确定性恢复 |
| 副作用派发 | 当前权限/撤销、正确宿主/代际、文件 precondition、取消与执行身份 |
| SDK 消费 | seq 域、批次连续性、幂等冲突、应用屏障、live 锚点与关闭 |

相同静态规则不重复铺到每个内部函数；跨信任边界和变化中的授权不因“前面检查过”而取消。
daemon 授权与客户端本机/OS 授权是不同边界，不能合并成一次服务端检查。
checksum/完整性检查也不能因 Rust 类型安全而删除；内存类型不证明磁盘数据正确。

### 2.3 错误、默认值与降级

- 必要写入、正文上传、commit、身份/资源绑定失败必须传播具名错误，不 log-and-Ok。
- display label、可选偏好、无业务影响的空统计可有明确默认；路径/凭据/存储/模型 endpoint 不做隐式替代。
- 缺结果不等于未执行，缺进度不等于完成；不从正文标签、UI 数组或 meta 推测业务终态。
- 恢复保留原 intent/receipt 与不确定状态；正常 read/query 不修业务状态、不唤醒执行。
- 降级只允许在明确可选能力上，保留 degraded/unavailable 语义；重要失败不能变成空集合或成功。
- 错误类型放在拥有该失败的边界，不为每个转发函数创建一套 wrapper enum。

生产错误处理遵循 AGENTS-x：不用 unwrap；测试断言可以 expect/unwrap 并提供失败上下文。
不能用 expect、通用 panic 或静默默认值绕过必要错误处理，也不把类型不可能的分支变成新的 fallback。
不机械禁止 clone/Arc/Mutex/trait；检查是否对应真实共享、所有权或适配边界。
trait 用于真实端口或替换需求，不为所有函数套一个可 mock 接口；禁止万能 service locator。

### 2.4 故障隔离与生命周期

任务/插件/子进程的失败隔离保留。它必须有具名 owner、cancel、join、typed outcome 和副作用恢复策略。
核心状态一致性未知时封闭相关准入、结算/对账，不 reset 为 Idle 后假设可以继续。
catch_unwind 仅用于明确隔离边界，不能证明共享状态或外部副作用已经恢复。
锁中毒后 into_inner 继续使用需要一致性理由；派生 cache 可丢弃重建，权威状态不能盲目沿用。
超时不等于任务停止；不能丢 JoinHandle、清 token/maps 或关闭 feed 后报告 Cancelled。
runtime/线程池由 composition root 管理和注入，不再靠模块内全局 runtime 补救环境依赖。
远端离线取消与迟到结果按 client-tools 契约处理，不把连通性检查变成新 session lease。

## 3. 测试规模与已核实样本

冻结 30 个 workspace 成员的只读统计：

| 范围 | tests/ 中 Rust 文件 | 物理行数 |
|---|---:|---:|
| workspace 总计 | 118 | 36,498 |
| qaqh-runtime | 39 | 12,845 |
| qaqh-session | 28 | 9,622 |
| runtime + session | 67 | 22,467，约占总计 62% |

统计直接读取 Cargo.toml 的 workspace.members，对各 member/tests/**/*.rs 计文件与 splitlines。
包含 fixture/helper，不包含 src 内联测试行数、其他语言/脚本，不是编译后的测试数，也不是可删除量。
这组数只作冻结基线，不承诺按某个百分比减测；不能拿它充当 coverage、运行耗时或维护价值。

两个典型旧机制测试：

- [晚到图片必须产生 SaveFull](../../../crates/qaqh-message/src/store.rs)：新线应验证附件正文正确持久和提交失败不提前确认，退出对全量重写机制的断言。
- [正常读取重复 msgid 必须 keep-first](../../../crates/qaqh-session/src/manager.rs)：新线验证重复提交幂等、同身份异内容冲突被拒；旧格式修复若保留，归 importer 测试。

幂等、完整性、恢复本身仍是需要的性质；旧 adapter 或缓存实现换掉时，不能连这些性质也删掉。
现有测试里同时包含算法与真实持久接缝，不把所有 BUG 注释/回归测试视为无用。

当前迁移还必须保留 [Chat](../../../crates/qaqh-runtime/tests/restart_request_body.rs) 与
[Responses](../../../crates/qaqh-runtime/tests/restart_request_body_responses.rs) 的真实 HTTP 请求体恢复性质，
以及 schema/参数别名与写冲突、compact revision/no-op/终态原因、usage 部分字段消费的反例。
请求体测试不同于内存上下文快照；只对固定配置/能力/序列化版本要求确定恢复，不把它扩成跨升级永不改变请求。
现有进程隔离/全局装配是待迁移测试设施，不作为新接口；这些测试存在不表示本轮执行通过。

## 4. 测试取舍

| 类型 | 新线规则 |
|---|---|
| getter、简单转发、编译器已保证的字段关系、照抄实现分支 | 删除无独立错误捕获能力的测试 |
| lease/driver、双 timeline、SaveFull、全局初始化补偿的专属测试 | 对应机制退出时同步退出；必要业务性质迁到新所有者 |
| 同一性质在 facade/adapter/manager/runtime 多层重复覆盖 | 放在实际拥有该性质的层；不同真实边界的接缝仍可各有测试 |
| patch/query、路径、provider parser、沙箱等成熟算法 | 保留有独立边界/反例的测试，合并重复 fixture |
| 跨语言 SDK / 协议消费 | 共享向量，各真实实现执行；不只跑参考模型 |
| commit/恢复/取消/权限/副作用去重 | 保留必要故障注入、并发与真实 IO 接缝 |
| 平台/交互终端/客户端执行 | 真实平台验收；不以 mock 的成功状态证明 OS 成功 |

测试位置服从责任：纯算法 unit 靠近实现，私有模块需要时拆同目录 tests.rs；跨公共边界 integration 放 tests/。
不为搬测试把内部状态变成 public，不新增巨大公共 TestContext 或万能测试运行时。
fixture/helper 只抽重复且稳定的输入与装配，不发展成第二套产品实现或业务逻辑。
每例使用独立数据根、显式实例和可控时钟/任务信号；避免依赖全局 OnceLock、共享 HOME/cwd 或 sleep 等偶然时序。
确需测真实进程环境/平台时使用受控进程隔离，不为了绕过共享全局状态把全部测试串行化。
快照只锁定必要公共契约，不断言整个内部 Debug dump 或无关字段/文案。
属性/模型测试只用于有价值的状态空间和不变量；无须为每个枚举/getter 引入新测试框架。

保留测试必须回答：保护哪条现行契约？能捕获什么独立错误？为什么在这一层？是否需要真实 IO？
删除测试必须回答：被保护的机制已退出，还是同一性质由更合适的测试覆盖？
答案应直接进入实施变更说明和相应测试，不创建长期测试删除账本或历史 bug 队列。

## 5. 施工出口与验证

允许在已验收切片内整段重写。一次能力迁移同时处理实现、真实调用者、补偿、fixture 和旧测试。
未迁移域可暂留原实现；临时单向 adapter 按 rollout 指定 owner、范围、删除条件及最迟退出版本，不长期留下兼容桥。
必要核心步骤：确认所有者/边界 → 消除非法状态来源 → 迁移真实消费者 → 退出补偿及旧专属测试 → 验证现行性质。
不能先删 guard 而留下双写/全局状态，也不能先删有效测试再宣称代码已简单。
beta4 测试是历史样本；近期业务/安全/平台回归是当前保留输入。新实现不必保持旧补偿机制，但必须保留其仍有效的业务性质。

审阅实现时检查：权威 writer/状态副本、fallback 路径、任务收割、依赖方向、真实边界校验和保真重放。
缩减来自上述机制退出；生产/测试行数和依赖数只作结果度量，不设删行或删测配额。
不为减少行数压缩格式、改巨大函数、加入万能框架或移走必要校验。

Rust 变更按 AGENTS-x 跑相关接缝、fmt、clippy、workspace tests；结果区分已运行/未运行/平台不可用。
完整检查通过后，仅在新增变更、失败或未解决疑点时重复/扩展验证。
图/schema/离线参考向量只验证各自设计范围，不能代替 commit/OS/网络/多语言 SDK/平台接缝。
本文写入不表示任何 Rust 机制或测试已删除，不声称现有全部测试通过。
