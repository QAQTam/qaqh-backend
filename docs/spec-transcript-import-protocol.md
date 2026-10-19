# 会话导入兼容与 IMP-06 协议

日期：2026-10-20。状态：本轮确定的实施策略，尚未实现；取代主设计中未定的兼容与入口部分。
范围：第一阶段 Claude Code 只读归档。Codex 和其他来源沿用同一外部协议，按能力发布适配器。

## 1. 发布与隔离决策

采用“新 daemon + 明确能力声明 + 独立只读归档入口”，保持现有 Ringing v2 envelope、
普通会话命令和在线回合事件语义不变。不升级整个 Ringing major，也不要求旧客户端忽略未知枚举。

1. `clients/open` 响应 capabilities 增加默认 false 的 `transcript_import`；只在导入服务、
   提交、归档读取和校验全部可用时返回 true。仅编译了 import crate 时仍为 false。
2. 导入命令走既有 `POST /ringing/v2/service/{method}`。新增 method 是本 ADR 范围内的局部扩展；
   不增加在线 ControlCommand、live 帧或新的 SSE channel。大文件原始取回仅新增下述流式 GET。
3. 第一阶段导入会话存放在数据根 `archives/{archive_id}/`，与 `sessions/` 分离。
   内部依旧使用 canonical identity、events 与 SessionBlobStore；archival writer/readers 由 session 所有。
   不先塞入普通 sessions，再依靠客户端隐藏；普通 session.list、恢复扫描和 worker 枚举都不包含 archives。
4. 普通 bootstrap/resume/send/driver 接口收到 archive_id 必须返回明确 archive_read_only，
   不得自动创建同 ID 的普通会话。所有加载入口共用分类与准入，不只在 UI 禁按钮。
5. 新客户端通过 transcript.list/get/messages/branches 浏览归档。旧客户端仍只见普通会话；
   不接收任何新增归档投影变体，不会把空 conversation 当成功恢复。
6. 新客户端连旧 daemon：能力缺失为 false，入口隐藏；直接调用不存在方法仍得到 unknown_method，
   不回退到 messages.jsonl、拼装普通 session.new 或上传到正文缓存冒充导入。

归档命名空间使旧 daemon 的会话 reader 不会读到新 fact，但不承诺旧程序会理解或备份 archives。
部署前完整备份数据根；回退程序时保留 archives 原样，禁止用旧版清理工具处理归档。
后续开放普通会话续聊需要单独 ADR，不能删除上述隔离后直接复用未知旧客户端。

## 2. 持久契约与 fact

- 共享持久契约定为 `qaqh-types::transcript`，schema=`qaqh.transcript/v1`。
  import 产出，session 校验和读取。session 不依赖 import 或 Claude JSONL parser。
- 当前 crate 内 `qaqh.transcript-import/v1` 是作业包格式，不直接成为已冻结的会话持久契约。
  迁移持久 DTO 时移出共享定义并删除重复类型；不做两套同形契约长期往返转换。
- `TranscriptImported` 作为新 FactPayload，只用于新 archives，schema_caps 包含 `transcript_archive.v1`。
  canonical envelope 保持现有 schema；这一新增枚举对旧 reader 不兼容，隔离解决访问兼容，
  而非宣称 serde 新增变体天然兼容。客户端不直接接收该磁盘 fact。
- 字段固定为 import_id、source_kind、source_session_key、package_ref、package_hash、
  normalization_hash、normalizer_version、completeness、imported_at_ms。
  import_id 是一次逻辑导入的稳定 ID，archive_id 对应 canonical SessionId；二者不混用作业 job_id。
- package_ref 指向持久 manifest：包含原始 artifact 分块、规范化分块、附件、来源图和覆盖报告。
  所有嵌套引用必须在提交前遍历校验并安装；只验证根哈希不够。
- 结构演进：新增可选字段提供默认值；改变既有含义必须新 schema。未知 schema/normalizer
  返回 unsupported_import_schema，不静默跳过，不用远端自动执行插件解码。
- 现有原始包 hash 不含全部清单信息，不用它代表完整 manifest 的安全摘要；
  fact 另以 package_ref 的 blob 内容哈希绑定完整已提交清单。原始内容去重身份与契约完整性摘要分开。

## 3. 任务与完整性

作业状态为 queued → preparing → ready → committing → completed；
queued/preparing/ready 可进入 cancelled/failed，committing 的取消按下述发布边界处理。
作业进入 ready 表示预览可读，不表示导入完整或已发布。

完整性独立返回 raw_bytes、messages、context 三项；枚举 complete/incomplete/unknown。
归档默认发布门槛：raw_bytes=complete 且 messages=complete；context 可为 unknown，因为第一阶段不可续聊。
messages 不完整时，commit 仅在显式 `allow_partial=true` 下发布且持久标记 partial；
raw_bytes 不完整、包结构/引用/哈希错误一律拒绝，allow_partial 不能覆盖。
缺工具结果若源本来中断可以作为 Unknown 历史状态；不得据此伪造工具成功或上下文完整。
归档浏览始终 `read_only=true`、`can_resume=false`，与 context 字段无关。

项目目录是批量作业，items 每项对应一个主来源会话和它的子代理；各自有 package_hash、
normalization_hash、预览、完整性和 archive_id。不能将整个目录转换成单条导入会话。
首版按 item 提交，任务汇总可 completed_with_errors，已成功 item 不因其他 item 失败回滚。

## 4. 方法表

下表方法名都在既有 service 路由下，不另造 JSON-RPC envelope。
写方法 require Admin + 有效 lease；读作业 require 同一身份所有权，lease 变化不改变身份。
首版归档查询与原始取回也 require Admin；设备远程授权可后续单独扩展。
本机路径指向 daemon 所在主机，不解释为远程客户端路径。

| method | 输入 | 成功回包 | 说明 |
|---|---|---|---|
| transcript.prepare | source_kind, source_path, allowed_roots, limits, idempotency_key | job_id, state, revision | 异步捕获并规范化；来源首版只认 claude_code |
| transcript.job | job_id, after_revision? | JobSnapshot | 立即返回状态快照；首版由客户端轮询，无新进度流 |
| transcript.preview | job_id, item_id | Preview | ready 后读取；绑定被冻结的两种摘要 |
| transcript.commit | job_id, item_id, package_hash, normalization_hash, allow_partial=false, idempotency_key | job_id, item_id, state, revision | 异步提交；已有相同归档可直接返回 completed 与 archive_id |
| transcript.cancel | job_id | JobSnapshot | 取消请求可重试，最终状态通过 job 查询 |
| transcript.list | cursor?, limit=50 | items, next_cursor | 已发布归档列表，按稳定分页顺序 |
| transcript.get | archive_id | ArchiveSummary | 标题、来源、完整性、能力、包 schema、导入时间 |
| transcript.branches | archive_id, cursor?, limit=100 | items, next_cursor | 主分支、其他分支、子代理及歧义 |
| transcript.messages | archive_id, branch_id, cursor?, limit=100 | items, next_cursor | 有序块与来源；大正文引用，绝不静默截断 |
| transcript.artifacts | archive_id, cursor?, limit=100 | items, next_cursor | 原始 artifact 标识、编码路径、长度、哈希 |
| transcript.diagnostics | archive_id 或 job_id+item_id, cursor?, limit=100 | items, next_cursor | 定位、影响级别、缺失/歧义说明 |

cursor 必须绑定归档/分支/契约修订，不接受来自其他查询的 cursor。
limit 上限 200；大内容按引用读取。源路径在 JSON 中只接受合法 Unicode 路径，
扫描发现的非 Unicode 子路径仍以持久编码保存；不得改用有损路径替代。
allowed_roots 必须与宿主既有授权相交，不因请求提供路径就扩张远程设备权限。

新增原始流式读取：
`GET /ringing/v2/transcripts/{archive_id}/artifacts/{artifact_id}`。
身份为 Admin，且有效 lease 具有该归档的只读绑定；查询服务成功读取时建立绑定，不加载 worker。
路径参数只查持久 manifest 的 ID，不拼接用户提供的源文件路径。
支持单段 HTTP Range、Content-Length、ETag（整 artifact SHA-256）及浏览器正确编码的 Content-Disposition；
只从持久 blob 读取，按原字节顺序输出。无 Range 时返回完整文件；无效范围返回 416。
正文内容引用使用现有 content GET 的持久 blob resolver，增加 archive scope 校验，不能仅依赖 TTL 缓存。

## 5. 回包与错误

JobSnapshot 至少包含：job_id、revision、state、phase、progress、items、error、created_at_ms、updated_at_ms。
item 包含 item_id、source_session_key、state、两种摘要、completeness、warnings_count、archive_id（可空）。
progress 只表示当前阶段的已处理量/已知总量，不制造跨阶段总百分比；全部字节计数固定使用十进制字符串，
时间戳、revision、条目数量限定 JS safe integer，不按数值大小临时改变 wire 类型。

Preview 示例（省略分页诊断）：

```json
{
  "job_id": "job-example",
  "item_id": "item-example",
  "package_hash": "<sha256>",
  "normalization_hash": "<sha256>",
  "source_kind": "claude_code",
  "title": "示例会话",
  "completeness": {"raw_bytes":"complete","messages":"complete","context":"unknown"},
  "read_only": true,
  "can_resume": false,
  "publishable": true,
  "warnings_count": 1
}
```

保持 service 错误外形 `code/message`，可追加 details；禁止将错误吞成 HTTP 200 空列表。
未完成操作接受时返回 HTTP 200 的上述状态回包，成功发布只能通过 completed + archive_id 判断。
method lookup 仍按现有 unknown_method/404；其余导入错误固定：

| code | HTTP | 含义 |
|---|---|---|
| invalid_body / invalid_envelope | 400 | 无效参数 |
| lease_required | 401 | 无有效 lease/未绑定 |
| forbidden | 403 | 无 Admin/作业所有权/来源根访问权 |
| import_not_found | 404 | 作业、item 或归档不存在 |
| import_not_ready | 409 | 作业尚未 ready 或提交状态不允许 |
| import_snapshot_mismatch | 409 | 提交与预览摘要不一致 |
| idempotency_conflict | 409 | 相同幂等键对应不同请求 |
| source_changed | 409 | 捕获期间来源变化 |
| archive_read_only | 409 | 将归档用于普通会话运行命令 |
| import_incomplete / corrupt_import_package | 422 | 完整性门槛不满足/结构哈希引用校验失败 |
| unsupported_import_schema | 422 | 不支持来源、包或规范化版本 |
| import_resource_limit | 413 | 大小或资源限制 |
| import_io_error | 500 | 捕获或持久 IO 故障 |

异步阶段错误存入 job/item.error，同样有 code/message/details，终态不得只写 log。

## 6. 幂等、取消与崩溃

- prepare 幂等键限定已认证身份及请求摘要；同请求重试返回原 job，不重新捕获活动源。
  要获取新的源快照使用新键。job/preview 不提供无限持久承诺；过期 job 返回明确 not_found。
- commit 绑定 preview 的两种摘要；归档去重键为 source_kind + 单来源会话包身份 + normalizer_version +
  normalization_hash。原始包相同但规范化修订不同须显式新导入，不覆盖。
- 同一原始包/相同适配器版本出现不同 normalization_hash 先按非确定性或篡改错误处理，
  不能自动制造多份归档。修复算法必须升级 normalizer_version。
- 会话提交使用未发布目录，blob → facts → 校验 → 原子发布至 archives；原子发布是提交线性化点。
  点前取消不暴露归档；点后取消返回 completed（含已发布结果），不能再报告 cancelled。
- 发布后更新 job 失败：通过发布事实和稳定 import_id 找回 archive_id，不重建第二份。
  宿主 operation receipt 将身份、幂等键、请求摘要和 import_id 持久关联；它是作业控制记录，
  不能成为归档内容权威。入队前持久化接受回执，重启对每条非终态作业给出恢复或失败终态。
- 作业暂存、回执与捕获目录在 ARCHITECTURE 登记为运行态/控制记录；既有 PendingCommandStore
  是否可直接承载要按实现审计，不能只因名字相似就假定其提供原子幂等语义。
- 不按客户端断连取消已接受作业；取消来自显式 cancel、宿主关闭或统一 CancelToken 根。

## 7. 发布门槛

1. 修复 [审查 R1–R4](review-transcript-import-2026-10-20.md)，新增期待正确行为的回归测试。
2. 完成共享契约和覆盖校验；发现诊断必须与捕获 manifest 绑定，不能依赖可删除的旁路 JSON。
3. 新 daemon/新客户端矩阵验证：旧客户端普通会话不受影响；新客户端连旧 daemon 不显示导入；
   直接拿 archive_id 调用每个普通加载/变更入口都被拒绝。
4. 新 reader 可恢复归档；未知包 schema 明确拒绝；删除来源和全部缓存仍能浏览及取回。
5. 批量会话隔离、并发判重、lease 过期后按身份重连、提交边界取消、发布各接缝崩溃均通过。
6. 首版更新桌面端与 daemon 联调后才开启 transcript_import；不开放继续对话。

此策略补齐设计决策，并不表示上述门槛已经达到。下一步可按此实现，无须再决定入口或兼容方向。
