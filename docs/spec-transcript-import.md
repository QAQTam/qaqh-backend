# 会话记录导入：qaqh-import 设计

日期：2026-10-20。状态：独立 crate 已实现，事实/查询/产品入口尚未接通；整体未验收。

本轮 [实现 review](review-transcript-import-2026-10-20.md) 复现四项需修复的问题。
后续兼容、发布与 IMP-06 采用 [已确定协议](spec-transcript-import-protocol.md)，其中的只读 archives
命名空间、完整性门槛和 service 方法表取代本文相应初稿；crate 内契约尚需移至共享层。

用户确定的阶段：第一阶段 Claude Code 消息无损导入，第二阶段 Codex，第三阶段其他来源。
本设计属于独立产品功能，不扩展 CLEAN 的清理任务；遵循 canonical log + blob 的事实源目标。
配套决策见 [ADR](adr/2026-10-20-transcript-import.md)。

## 1. 目标与无损定义

新增 `crates/qaqh-import`，统一处理外部会话的发现、快照、解析、规范化、校验和来源追踪。
来源差异放在 crate 内的适配器中，qaqh 正常读取路径只消费已提交的当前格式。

“无损”必须分开报告三项，不能仅凭“备份了 JSONL”宣称消息已无损导入：

| 维度 | 第一阶段要求 | 验证方式 |
|---|---|---|
| 原始保真 | 所选范围内每个文件的字节、顺序、行结束符、BOM、未知字段、重复记录、坏行全部保留 | 从会话 blob 重建文件，长度和 SHA-256 与捕获快照一致 |
| 消息保真 | 可识别消息的块顺序、文本、思考、签名、工具参数/结果、图片、原 ID、时间、父链、分支、子代理归属完整表达 | 逐记录/逐块覆盖检查，原始位置与规范化对象双向关联 |
| 语义完整 | 分支、压缩、内容替换等已识别语义正确；未知或缺失内容明确可见 | 输出能力与缺失报告；有语义缺口时禁止标为完整导入 |

不承诺仅凭消息文件恢复 Claude Code 的运行进程、权限会话、Hooks、MCP 连接、文件 undo、任务调度或模型缓存。
第一阶段交付导入、查看、来源定位、原始文件取回和重启重建；“继续对话”是另一个验收项，默认不开放。
显示布局和模型协议重放不属于字节级往返；原始存档取回才要求逐字节一致。

## 2. 第一阶段范围

输入支持单个 transcript JSONL 和 Claude 项目目录。默认目录由宿主解析
`CLAUDE_CONFIG_DIR`，未设置则使用用户目录下的 `.claude/projects`；crate 接收显式路径，不自行读取全局环境。

会话包包含：

- 主 transcript 的全部记录及全部分支，不只最新主链。
- 对应 `<sessionId>/subagents/` 下的 transcript 与相关元数据，包括子目录。
- 消息明确引用、位于所选允许根内的本地正文附件及外置工具结果。
- 标题、摘要、版本、cwd、Git 信息、模式、压缩、内容替换及辅助记录。

不递归打包整个 `.claude`、项目工作区、密钥配置或任意外部路径。文件历史记录本身保留；
记录提及的工作区文件不因此变为附件。远程引用原样保存，不自动联网获取。
绝对路径、越界路径、符号链接或重解析点越界的引用记为未解析，用户可通过后续明确选择补入。

发现阶段枚举输入文件并冻结清单；读取期间新出现的文件不加入本次包，在报告中标记快照边界。
普通路径字符串本身只作为来源数据保存，不触发切换目录、运行命令或恢复 worktree。

## 3. crate 职责与依赖

箭头表示依赖，以下均为目标设计，尚未增加 Cargo 依赖：

```text
qaqh-runtime / daemon composition
    -> qaqh-import -> qaqh-types
    -> qaqh-session -> qaqh-types

qaqh-import 不依赖 runtime、daemon、ringing、session 或 provider SDK
qaqh-session 不依赖 qaqh-import 的来源解析器
```

- `qaqh-import`：源格式和跨来源算法的所有者；产出可验证的导入包，不创建正式会话，不启动 actor。
- `qaqh-types::transcript`：跨 crate 的持久导入契约，包含来源引用、记录图、内容块、覆盖与完整性描述；
  不放扫描器、路径解析或文件 IO。只在真正需要共享的类型稳定后加入，避免提前搭通用框架。
- `qaqh-session`：导入包 blob 安装、引用校验、事实提交、归档与时间线投影、原始内容取回。
- 宿主服务：取消、进度、权限、任务编排、目录选择、正式会话发布；不在 HTTP handler 中解析 JSONL。

独立 crate 的理由是外部格式变化与会话存储变化具有不同所有者；实际接缝是
“来源文件 → 可验证包”和“可验证包 → 会话事实”。Codex 增加适配器，不让 session 增加第二套解析器。
已有 MCP 配置导入继续归 config，本 crate 不接管配置或身份迁移。

建议目录：

```text
crates/qaqh-import/
  Cargo.toml
  src/
    lib.rs
    model.rs                  # 导入作业选项、计划、结果
    error.rs                  # 有类型的错误和诊断位置
    capture.rs                # 稳定字节快照、分块与哈希
    package.rs                # 包清单、包读取、完整性校验
    coverage.rs               # 记录/块处置覆盖检查
    adapters/
      mod.rs                  # 显式分发，不做动态插件注册
      claude_code/
        mod.rs
        discover.rs
        records.rs
        graph.rs
        normalize.rs
        attachments.rs
  tests/
    fixtures/claude_code/
    capture_roundtrip.rs
    claude_graph.rs
    claude_content.rs
    package_integrity.rs
```

第二阶段实际实现时增加 `adapters/codex/`；第三阶段按真实样本扩展。第一阶段不创建空适配器。
初始依赖预计为 serde、serde_json、thiserror、sha2 与 qaqh-types；不引入 Node/Bun 或 Anthropic SDK。

## 4. 包与规范化数据模型

### 4.1 原始字节层

`SourceArtifact` 保存包内相对路径、文件种类、字节长度、整文件 SHA-256、按序分块引用与捕获信息。
路径必须可往返表达原平台路径；显示字符串与可还原路径编码分开，不能只保存有损 UTF-8 转换结果。
原始记录定位使用 `(artifact_id, byte_offset, byte_length, ordinal)`；行号只用于显示。
任意非 UTF-8、空行、CRLF、末尾无换行、重复 JSON 键也不会因解析而改变原始字节。

原始文件按固定大小字节块捕获；记录可以跨块，范围读取器必须拼接。超长行允许转入磁盘处理，
不能以“超过长度上限”直接跳过。所有资源限制返回显式错误或完整性降级，不截断为成功。
包输出采用临时目录与磁盘对象池，结束时固定清单；不是把整个包塞进一个 `Vec<u8>`。

### 4.2 语义层

核心概念：

| 类型 | 含义 |
|---|---|
| `SourceDescriptor` | 来源种类、适配器版本、来源程序版本（若已知）、源会话标识 |
| `RecordOrigin` | 原始记录位置及内容块路径，如 `message.content[2]` |
| `TranscriptRecord` | 每条物理记录的身份、解析状态与语义对象引用 |
| `ImportedMessage` | 来源角色、原 UUID/模型消息 ID、原始时间、顺序内容块、参与者及可见性 |
| `ImportedBlock` | 文本、思考、工具调用、工具结果、图片、文档、Opaque；大正文使用包内容引用 |
| `TranscriptGraph` | 物理父链、逻辑父链、分支、主会话/子代理关联及有效上下文描述 |
| `RecordDisposition` | 已转消息、已转元数据、未知保留、损坏保留、冲突保留；可一对多 |
| `ImportCompleteness` | 原始保真、消息保真、上下文可恢复能力及缺失项 |

`ImportedBlock::Opaque` 保留未知块的完整原始范围和类型名；不能将它无声转成空文本。
reasoning 必须保留明文、signature、redacted/encrypted 原始内容及类别；不可展示内容默认不展示。
工具 input 保留原始 JSON 范围，解析后的 Value 只是查询视图；避免数字精度、重复键和字面量格式丢失。
工具结果保留原始内容块序列、错误字段与扩展输出，不能先格式化为最多若干字符的摘要。

所有物理记录都生成稳定的包内 ID；原 UUID 不作为唯一主键。重复 UUID 的相同/冲突记录都保留，
图重建使用明确的来源规则并记录选择依据，无法唯一决定时输出歧义。
合并 assistant 展示行只产生投影，不删除或覆盖源消息与源块。

### 4.3 ID 与时间

包内 ID 按来源、文件身份和物理位置确定；记录移动会改变 ID，这是不同快照的预期行为。
正式 qaqh SessionId/LogId 由 session 分配，不直接复用 Claude UUID。
现有 fact 对工具、回合等 ID 有格式校验；需要生成正式 ID 时保留独立映射，不能原样塞入源 ID。

来源时间原样保留，同时可派生解析后的时间戳。缺失或不合法时保存原因，不伪造来源时间。
fact 的提交时间与来源消息时间分别表达；显示排序以图和物理顺序为依据，不单凭时间排序。

## 5. Claude Code 适配规则

1. 扫描 JSONL，识别外层 `type` 与 `message.content` 的字符串/数组两种形式。
   将原始文件完整捕获后再做规范化，预览和提交消费同一快照。
2. `user` 记录中的 `tool_result` 是工具结果，不计为新用户提问；混合文本、图片和结果仍逐块保留。
   `isMeta`、`isVisibleInTranscriptOnly`、`isCompactSummary` 保存为语义字段，不能只凭 role 判断可见性。
3. 使用 `parentUuid` 重建物理链，`logicalParentUuid` 单独保存。循环、悬空父节点和冲突必须报告。
   历史 progress 节点保留在原始图，派生对话链可桥接它们；不把 progress 伪装成持久对话正文。
4. 同一 `message.id` 的 assistant 记录构成关联组。保留所有成员及块顺序，
   按工具调用 ID 和来源父节点补回并行工具结果；不可只走一条父链后丢弃兄弟节点。
5. 全部分支保留；默认展示最新可判定的主分支，其他分支可查询。
   `isSidechain` 及独立子代理文件形成独立参与者/子记录，不掺入主会话正文。
6. 压缩边界、保留片段、snip、context-collapse、content-replacement 分别建模。
   人类归档显示原消息；有效上下文是单独的派生视图，保留摘要与覆盖范围。
   不完全理解的压缩变体保留原文并阻止“上下文完整”标记，不能自动退回全部历史。
7. 标题兼容核心 `customTitle / aiTitle` 和 occ Electron 的 `title` 字段形态。
   展示优先用户标题、AI 标题、首条有效提问；最后提问保留为独立元数据，不能覆盖用户标题。
8. 外置工具正文、图片、文档和其他本地附件归档为包内容对象；缺失引用明确列出。
   原始签名及隐藏协议块保留，不假设可以跨 provider 重放。
9. 模型 usage、stop_reason、错误、请求/模型消息 ID 等来源字段完整保留。
   多个拆分记录共享的 usage 不重复累计；无法确定统计口径时保留原值并标记不可聚合。
10. 源日志没有明确证明的工具/回合终态为 Unknown；不得一律标为 Done/Completed。
    历史导入不产生可执行 ToolIntent、等待审批、等待回答或自动恢复任务。

第一阶段必须提供上述已知格式的覆盖样本；仅存原始文件但缺少这些已知消息语义的实现不算完成。
完全未知记录允许以 Opaque 导入；报告区分“未知元数据”和“可能影响消息/上下文的未知记录”。

## 6. 公共 API 草案

以下是职责草图，不是已存在的 Rust API；细部签名在实现时由资源与取消接口收敛。

```rust
pub fn discover(request: DiscoverRequest<'_>) -> Result<Discovery, ImportError>;
pub fn capture(request: CaptureRequest<'_>) -> Result<CapturedSource, ImportError>;
pub fn prepare(request: PrepareRequest<'_>) -> Result<PreparedImport, ImportError>;
pub fn verify(request: VerifyRequest<'_>) -> Result<VerificationReport, ImportError>;
```

请求对象显式携带允许根、输入、作业目录、大小限制和宿主取消/进度上下文。
取消使用仓库统一 CancelToken 树；若现有 token 归属层阻止依赖，先将最小共享取消契约放到合法底层，
不能引入独立 atomic flag、全局状态或导入专用取消语义。

`CapturedSource` 是冻结字节快照；`PreparedImport` 是有所有权的磁盘包句柄和报告，绑定原始快照哈希、
适配器版本、选项、全部规范化内容及诊断。提交时不得再次解析原始路径。
包内部用本地对象引用；session 安装后转换为正式 ContentRef，不能把未安装的哈希当可解析引用。
来源类型使用显式枚举和静态分发，Codex 有样本时再增加变体。

错误至少区分 IO、来源变化、未支持来源、包损坏、无效图、资源超限、取消、引用缺失与提交冲突。
单条坏行会生成定位诊断并保留字节；缺少必要消息语义时结果为不完整包，不能返回“完整成功”。
操作结果区分 `Complete / Incomplete`；取消、IO 失败和校验失败使用 Err。未完成包不发布为普通会话。

## 7. canonical 集成

新增来源无关事实 `TranscriptImported`，字段建议：

```text
import_id
source_kind
source_session_key
package_ref                 # 不可变包清单，引用原始及规范化分块
package_hash
normalizer_version
completeness
imported_at_ms
```

这是事实协议变更，必须同步修改校验、projection slot、重放、读取及客户端类型；
不能借用 WorkspaceResourceChanged 或 SessionMetadataChanged 隐藏任意新事实语义。
完整 manifest 保存文件身份、规范化图根、块列表、覆盖、来源定位、分支与诊断；
本地绝对路径只作为来源信息，不作为恢复时的读取地址。

session 在已校验包上执行：

1. 新建不对会话列表发布的 staging 会话与 canonical identity。
2. 安装原始分块、规范化分块、附件和包清单到 SessionBlobStore；逐个校验内容哈希与引用闭包。
3. 按 writer lease/fence 追加 SessionCreated 与 TranscriptImported，提交前持久化全部引用。
4. session 负责从导入契约生成只读归档/分支/时间线投影；投影代码不认识 Claude JSONL。
   大对象通过 session 读取层按需解析，不能在纯 reducer 中执行文件 IO。
5. 完整性验证通过后原子发布会话，再更新可重建索引并通知客户端。

若只有原始字节完整、语义不完整，保留可恢复包并向用户返回报告；不能冒充完整普通会话。
后续可显式发布为“部分导入”，但状态必须持久化且前端可见，不以日志 warning 代替。

不把历史导入转换成伪造的在线 TurnStarted/ToolIntent 序列。
时间线应能表达 Imported/Unknown；导入语义中的历史工具结果不进入运行恢复账本。
后续支持继续对话时，由模型上下文投影消费被选择的导入分支，新的真实回合使用正常事实。
这一步必须验证压缩、协议签名、工具配对和当前工具能力，不能因包可查看就默认可继续执行。

当前代码仍以 messages.jsonl 恢复模型上下文，不能仅追加新 fact 就声称已打通 resume。
本功能不新增 messages.jsonl/meta 的权威双写。归档展示可独立接通；开放继续对话前须完成
canonical 上下文读取接缝或与 CLEAN-3 一起切换，不能以长期兼容桥替代。

## 8. 一致性、幂等与恢复

- 对活动文件默认检查捕获前后文件身份、长度和修改信息，并二次核对读取内容；变化返回 SourceChanged。
  不宣称普通文件读取提供跨文件原子快照；多文件包记录各自捕获点，需一致语义的来源先停止写入再捕获。
- 包身份由排序后的文件清单、路径身份和内容哈希决定，不能只用 sessionId 或 mtime。
  同包重复提交默认返回已导入结果；同源 ID、不同内容是新快照，保留两者，不覆盖已有会话。
- 包身份与规范化版本分开。新适配器重解析同包需显式创建新的导入修订，记录原导入关系，不能偷偷重写事实。
- 并发提交在 session 的导入发布接缝串行判重；判重索引可由已提交事实重建，不成为第二事实源。
  发布后、索引更新前崩溃，重启应补齐索引且避免产生第二个导入结果。
- staging 在发布前不可被 list/resume/worker 看见；新会话中部分事实已写入也不表示产品层导入成功。
- 提交中断后只保留隐藏 staging；通过已验证清单完成或清理，再发布。已发布会话永不就地覆盖。
  自动清理只针对本作业拥有的隐藏目录，删除前验证边界。
- 外置附件缺失与不可理解的消息语义都是报告数据；不得补空内容、伪造成功或 silently skip。

## 9. 存储登记

| 数据 | 性质 | 写入者 | 恢复/回收 |
|---|---|---|---|
| TranscriptImported 事实 | canonical 权威 | session 导入提交端口 | canonical committed prefix |
| 原始文件、附件、规范化契约、manifest blobs | canonical 正文 | SessionBlobStore | fact 引用闭包；无 TTL，随会话回收 |
| 导入浏览/分支/时间线索引 | 投影 | session | fact + 包契约重建 |
| 跨会话包判重索引 | 缓存 | 宿主/session 服务 | 已发布会话事实重建 |
| 捕获作业目录、未发布 staging | 作业运行态 | import/宿主、session 各自拥有 | 可恢复或清理，不可作正式历史来源 |

正式会话不新增作为事实源的 `source.jsonl`、`import.json` 或独立 SQLite 数据库。
包中的原始文件可以按原样取回；取回生成的是导出产物，不是另一份持久事实。
现有 blob API 按完整 Vec 读写；大块流式安装、引用闭包验证、staging 发布、导入投影和取回端口
都是需要实现的接缝，当前代码不能直接假定具备。

## 10. 第一阶段任务与验收

| 任务 | 交付 | 验收 |
|---|---|---|
| IMP-01 契约与语料 | 公共契约、schema 版本、Claude 脱敏金标、适配器覆盖矩阵 | 每个已知记录/块有处置规则；未知项可定位 |
| IMP-02 crate 与捕获 | 建 crate、目录发现、稳定快照、磁盘包、原始取回 | CRLF/BOM/坏行/重复键/非 UTF-8/无末尾换行逐字节往返 |
| IMP-03 Claude 规范化 | 消息图、分支、子代理、工具配对、压缩与附件解析 | 金标中消息/块顺序、所有分支与上下文边界一致 |
| IMP-04 session 提交 | blob 安装、新事实、验证、隐藏 staging、幂等发布 | 故障注入不暴露半成品；所有引用可解析；并发重复不重复发布 |
| IMP-05 查询展示 | 导入列表、主分支/其他分支、原始来源与报告、取回 | 删除投影、重启、删除源目录后仍可完整读取和取回 |
| IMP-06 产品接入 | 预览、导入、进度、取消、明确终态与完整性显示 | 单文件与项目目录流程可用；没有 worker/工具执行副作用 |

重点测试语料：并行工具、拆分 assistant、多文本块、混合 user 内容、工具错误/缺结果、
thinking signature/redacted、图片/文档、未知内容块、重复/冲突 UUID、旧 progress、循环/悬空链、
rewind 分支、压缩保留段、snip、context-collapse、content-replacement、标题字段变体、子代理、
缺失外置内容、活动文件变化、超长行与大包、提交各接缝崩溃、取消与并发重复。

覆盖不变量：每个原始字节范围能取回；每条物理记录有处置；每个识别出的内容块有规范化对象；
每个规范化对象能回到来源；主分支展示与归档存量分别核对。
纯 crate 测试不接真实账号；产品集成另跑真实脱敏固定快照。大包报告吞吐、峰值内存与磁盘用量，
不能用截掉较老消息或限制展示条数冒充导入完成。
实现时运行 fmt、相关 crate 测试和 workspace 必要检查；本设计提交只校验文档，不声称上述测试已通过。

## 11. 后续阶段

第二阶段先检查 Codex 实际 rollout/session 格式与版本，建立独立金标，然后添加 Codex 适配器。
保留其原始事件、工具、分支/压缩、协议状态和来源字段；不预设与 Claude 一一对应。
复用捕获、包、覆盖、提交、判重及查询接口，来源专属数据继续通过保真扩展表达。

第三阶段对每个新来源提供明确能力矩阵、格式样本、版本、Opaque 规则和金标，再接适配器。
只有两个以上真实来源暴露同一语义时才抽取共用转换；不构建“任意 JSON 自动猜角色”的通用导入器。

## 12. 源码依据

本次分析基线：qaqh `37167d74b96fdef0700f5ca886475fb59e76d238`；
occ `3c58bfe5fdf1a66bdb5ca2091f3c5878cfd1a448`。
occ 是本机 restored 源码，兼容性结论限于此版本和验收语料，不代表所有 Claude Code 版本。

- occ `src/utils/sessionStorage.ts`：原始路径、父链、压缩重连、并行工具结果恢复、元数据分发。
- occ `src/utils/sessionStoragePortable.ts`、`src/utils/envUtils.ts`：目录命名与配置根。
- occ `src/types/logs.ts`、`src/utils/sessionRestore.ts`：记录契约、标题字段、恢复附属状态。
- occ `electron/backend/replay.ts`：简化 UI 回放，省略思考且不完整处理分支，不能作为无损导入器。
- qaqh `qaqh-types/src/message.rs`：现有消息类型不足以完整表示原始来源与图。
- qaqh `qaqh-session/src/session_fact_v2/{types,validation}.rs`：事实类型与 ID 约束。
- qaqh `qaqh-session/src/canonical/{store,blobs}.rs`：持久事实与正文提交能力。
- qaqh `qaqh-runtime/src/agent/state/lifecycle.rs`：当前模型恢复仍走消息归档。
- [架构基线](ARCHITECTURE.md)、[CLEAN-3](spec-architecture-convergence.md)：唯一事实源目标与当前差距。
