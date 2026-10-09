# 诊断模块与前端 API 盘点

日期：2026-10-09。范围：当前后端源码静态审查；未运行 provider 请求或修改业务实现。

## 结论

有必要统一诊断配置、附加采集和聚合查询，但不应搬走各层的业务事实，也不必新增独立 HTTP/SSE 协议或立刻增加 crate。建议先在 qaqh-runtime 建立 diagnostics 模块，通过既有 Ringing v2 service 暴露类型化查询，由 daemon 组装采集端口和配置。gate 负责 provider 数据解释，session 继续拥有 canonical facts。

必须区分三件事：业务事实（始终保留）、诊断聚合（可关闭）、敏感 trace（独立明确开启）。关闭诊断不得停止解析 token、持久化正常会话用量、记录工具结果或改变权限/恢复行为。

## 当前实际分布

| 项目 | 代码位置 | 现状与归属 |
| --- | --- | --- |
| provider 输入/输出/cache/reasoning token | qaqh-gate/src/{openai,responses,anthropic,gemini}_sdk.rs；qaqh-types/src/api_types.rs | 已有统一 UsageInfo；不同 provider 的归一化应留在 gate |
| 会话累计与最近用量 | qaqh-types/src/session.rs::record_usage；qaqh-runtime/src/agent/turn_lap/gate.rs | Done 累计并排队 PersistUsage；业务事实，不属于可随意关闭的 debug |
| 前端会话用量契约 | qaqh-domain/src/state.rs::ConversationState | 已有 usage、usage_totals、usage_requests、cache_reported_requests，不需要复制另一套权威值 |
| 日 token 统计写入 | qaqh-runtime/src/agent/util/telemetry.rs | 每个带 usage 的 Done 同步追加全局 token_stats.jsonl；无统一开关；失败静默 |
| 日 token 查询 | qaqh-runtime/src/service/plan.rs::token_stats；service.rs；ringing/service_methods.rs | 已有 stats.token_usage，走 POST /ringing/v2/service/{method}；与 plan 混居，查询全文件 |
| 工具结果与耗时 | qaqh-types/src/tool_result.rs；qaqh-session/src/session_fact_v2/types.rs::ToolFinished；qaqh-runtime/src/agent/tool_runtime.rs | 状态、error、metrics 已有事实源；适合派生错误率，而不是各工具另写计数器 |
| 工具展示 metrics | qaqh-runtime/src/timeline.rs::apply_result_metrics | 已投影 elapsed_ms、output_bytes、retry_count 等，缺少全局/窗口聚合 |
| gate 流 trace | qaqh-gate/src/transport.rs::SseTrace | QAQH_SSE_TRACE 指定文件；记录派生事件顺序/时间/类型及部分长度，不是原始 SSE，也不记录 usage 的数值 |
| 请求快照 | qaqh-runtime/src/agent/engine_turn.rs::dump_request_log | QAQH_REQUEST_LOG=1，OnceLock 缓存开关；写 request-log.jsonl，含消息正文，不宜直接通过普通诊断 API 暴露 |
| 故障注入 | daemon/src/axum_server/axum_impl/test_hooks.rs；runtime/src/test_hooks.rs | daemon 已有集中 TestHooks；这是测试控制面，不应与观测查询混成可由前端随意启用的功能 |

## 现有具体问题

1. **日期口径不一致**：telemetry 写入使用 chrono_local_date（固定 UTC+8），plan::days_before_today 使用 UTC 日。UTC+8 每日 00:00–07:59 查询窗口可能不包含已经写入的当天记录。
2. **缺失与零命中混淆**：UsageInfo 已有 cache_usage_reported，SessionMeta 已有 reported 请求数，但 token_stats.jsonl 未保存该字段，聚合无分母时直接给 0%。API 应返回 null/unsupported 与 coverage，不能把未知表现为真实 0%。
3. **记录粒度不清晰**：注释写 per-turn，实际调用位于每次模型请求的 Done；同一 turn 多 round 会产生多条。统计 calls 应明确为 model_requests，不是用户 turn 数。
4. **附加 IO 在执行路径**：token 日志和 trace 使用同步文件写入；没有统一保留策略、大小限制、丢弃统计或采集健康状态。days 虽封顶 366，但读取成本仍随整个日志增长。
5. **缺关联键与来源覆盖**：全局 token 日志无 session_id/turn_id/request_id/provider 标识，也没有明确区分主请求、标题、compact、子代理等范围。不能宣称当前日统计覆盖所有模型调用。
6. **流式 usage 接线需专项确认**：gate.rs 的 UsageUpdate 分支和 Done 的所谓“终值补发”目前只更新本地节流变量，没有在这两个分支实际 emit 用量事件。不能仅凭注释断言前端已实时收到 usage；终态和快照路径应与客户端联合验证。
7. **错误率没有统一定义**：ToolStatus::is_failure 包含 Error、Partial、Cancelled，不能直接等同“工具执行错误率”。权限拒绝、取消、后台执行与恢复对账应分别统计。

## 建议边界

```text
gate provider adapter -> UsageInfo / request observation
tool runtime          -> canonical ToolFinished
session               -> facts + existing business projections
                              |
                       DiagnosticsPort
                              |
                 bounded collector / derived index
                              |
                 diagnostics service -> qaqh-client -> desktop
```

采集点分布是正常的，集中的是契约、配置、存储与查询。不要让 diagnostics 反向成为 session/tool 的事实源。可重放的统计从 facts 派生；仅存在于运行期的首块延迟、重试、连接背压等从端口观测。跨 crate 端口放在双方可依赖的既有中立边界；没有实际需要前不增 crate。

推荐采集模式：off / basic / trace。off 停止额外计数、索引和诊断文件写入；basic 只采元数据；trace 仍默认不采正文，请求正文快照单独显式授权配置。模块提供 no-op 实现，fork 无需逐处删代码。首期使用启动配置即可；若后续支持热切换，需统一配置快照和 generation，不能沿用各处 OnceLock 与逐次读环境变量的混合行为。

故障注入保持独立 TestHooks，只共享配置清单/文档，不接入用户诊断开关。正常错误日志及安全审计也不应被这个开关无意关闭。

## API 建议（拟议，尚未实现）

复用 POST /ringing/v2/service/{method}，不新增诊断 SSE，不把诊断事件写进 canonical 会话流：

- diagnostics.status：有效模式、能力、运行/历史范围、采集错误和 dropped 数、配置 generation。
- diagnostics.session：session_id、时间窗口；usage/cache/tool 聚合、coverage、as_of 与事实水位。
- diagnostics.summary：全局窗口统计，只允许 admin；会话查询保持现有 lease 归属检查。

typed DTO 放入现有共享契约层，走既有 TS 导出；前端已拆到同级 qaqh-desktop-app，此轮不改前端。stats.token_usage 先兼容原返回，底层委托新模块；新 API 使用 nullable rate、明确单位与分母，不静默改变旧百分比契约。

每个查询应说明 scope、window、sample_count、coverage、source（provider_reported/estimated）、as_of、enabled。off 不伪造全零：status 明确 disabled；若允许读已有历史，则标明采集已停及陈旧时间。缓存命中这里专指 provider prompt cache，与本地文件缓存/内容缓存另命名。

工具至少分 succeeded、failed、partial、cancelled、denied、backgrounded、unknown/reconciled。首期分别给逻辑 call 和 execution/attempt 计数；去重键基于既有 call_id/execution_id/事实水位，而不是 SSE 到达次数。后台任务的启动确认不直接视为最终完成。没有可用分母时 rate=null。

usage 流式更新是累计快照，按 request_id 替换，不逐块相加；Done 最终值只提交一次。未完成、失败或取消但已报告用量的请求单独保留 observed_partial，不能当作完整账单，也不能直接抹掉。reasoning 是否包含在 output 中遵循各 adapter 现有口径，不再次加总。

## 分期落地

1. **小规模收敛**：把 token_stats 从 plan 拆出，与 telemetry 写入归到 diagnostics；集中配置；保持旧 stats API；修日期/unknown cache 口径；增 status 与 typed DTO。默认兼容当前日用量统计行为，fork 可一处关闭。
2. **工具聚合**：订阅 committed ToolFinished，建立可再生索引与水位，支持会话与窗口查询，覆盖权限拒绝、重试、恢复和后台执行口径。
3. **运行期观测**：gate 请求关联键、延迟/重试/SSE 元数据，后台有界队列、保留与轮转策略，关闭或队列满不能阻塞业务。
4. **前端接入**：复用 qaqh-client service 调用和现有会话 usage；增加诊断页面，不在 renderer 重算权威错误率或缓存命中率。

关键验收：off 下无额外诊断文件且业务用量/事实仍正常；UTC+8 日期边界；未知 cache 与零命中区别；多 UsageUpdate + Done 不重复；重连重放/恢复对账不重复；取消/拒绝/后台不混入失败分母；采集失败和队列满不影响业务；全局权限和会话归属；历史缺字段兼容。无需为本轮纯盘点运行 workspace 测试。
