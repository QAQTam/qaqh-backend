# config/config-api 重构施工规格：CFG-02

日期：2026-10-20。状态：施工规格草案；CFG-01 已在工作树落地，CFG-02 尚未实施。
授权目标：配置契约优先服务前端适配、后端服务管理；允许整段替换，不以长期补丁/双权威换取表面兼容。
本文是下一阶段唯一任务规格；现状交接见 [handoff](handoff/2026-10-20-config-resources.md)，
现行 CFG-01 契约见 [configuration](../new-tech/docs/v2-reset/configuration.md)。

## 0. 约束、状态与评审门

- 必须先读根目录 AGENTS-x.md；它高于本规格。用户明确禁止调用子代理，含继续/唤醒已存在子代理。
- 每个任务只提交自己的范围；不得清理接手时的未提交修改，不修改生产配置、账号和用户数据根。
- CFG-01 的读取 API 已存在；CFG-02 的契约/磁盘格式/方法在本文中均为目标，不能写成已经可用。
- 新方法或旧方法语义变化、新存储及依赖边先更新 ADR 与架构表；不得绕过 wire 冻结。
- **G0 设计评审门**：CFG2-00 产物获所有者/指定 reviewer 批准后，才能执行 CFG2-02 及之后任务。
  CFG2-01 可先做纯契约与离线向量，不注册生产路由、不发布未实现能力。
- 本文固定行为要求和施工拆分；§5 的存储候选是待批准方案，不是让便宜模型自由挑选的菜单。
  未批准或与真实代码冲突时停止，返回具名问题，不自行选 SQLite、在线双写或隐藏 fallback。

## 1. 目标与非目标

### 1.1 必须交付的前端行为

1. 获取完整配置目录，直接编辑非默认 profile；不经过 profile.apply→config.save。
2. 创建、发布、删除指定资源与改变新会话默认分别是命令；打开/选中列表行没有副作用。
3. 两端从同一 revision 编辑同一资源，第二次保存明确冲突，可拿当前脱敏资源重新编辑。
4. 主机设置、模型 profile、凭据变更、客户端偏好分离；secret 不靠 `****`/空串表达命令。
5. 回执区分“已持久保存”与“actor 已应用”；本阶段不伪造 applied/pending。
6. 列表、详情、错误及 TS/native 类型一致，客户端无需手写方法名或猜测 JSON。

### 1.2 必须交付的后端行为

一个数据根一个配置 repository owner；一个提交队列决定版本、CAS 与发布顺序。
所有生产写入口走它，包括旧 config.save/profile.*、权限写入、外部文件导入和 CLI。
读取是无副作用查询；配置广播是失效通知，不是第二份权威槽。
主机执行/集成上限与模型选择有独立通知，后台服务不为一次模型编辑全部重启。

### 1.3 不在本阶段

- 会话 selection facts、确切版本恢复、actor pending/applied、完整 child/main/compact/title 快照：CFG-03。
- Provider 登录/refresh 协议、账号 manager 去全局、真实账号主体迁移：独立 provider 切片。
- Ringing major 3、session-store/WAL/loop 重写、市场/ACP/ClientToolHost、全局 TLS 清扫。
- 桌面/Kotlin/ArkTS 实际界面改造不在本仓默认修改范围，必须另指派跨仓任务。
- 不以本阶段完成宣称“默认变更不影响所有旧会话”。旧 None/profile-name 选择仍是 CFG-03 待迁移语义。

## 2. 起点及需要退出的结构

以符号为准，行号可能漂移：

| 位置 | 当前结构 | CFG-02 要求 |
|---|---|---|
| config/src/config.rs::Config | profile map、active、扁平模型、明文内存 key、UI、服务设置混合 | 只作未迁移 runtime 的投影，不当配置 writer |
| Config::update/config_io_lock | static 锁中 load→mutate→save→publish | 实例提交队列；锁不跨 IO |
| Config::save_with | 重建 profiles[active]，写 Main/Subagent secret | 退出资源持久写入；不借完整运行配置覆盖 repository |
| Config::apply_profile/for_session | 手工字段复制、缺失 profile 回退 | 管理路径不再调用；运行解析在 CFG-03 处理 |
| runtime/service.rs::profile.save_current | 队列外读 key、改 secret、随后更新配置 | 一个命令内固定来源；凭据先版本化再发布引用 |
| Config::save/CLI mcp import | 可绕开 service 的持久入口 | 正式服务态不可绕过 repository；离线入口受同根互斥 |
| config/watch.rs | static channel/latest，authoritative 读失败回缓存 | 生产配置域切到实例读及通知，不增加新全局槽 |
| ConfigReader | 无 revision 的 config.toml 只读投影 | 切换后只读新 owner 的资源，不留下第二份数据根权威 |

CFG-01 未修复 credential 字段遗漏、全局 key 继承、子任务全局配置读取或 persist_profile 忽略 IO 错误。
不能把本阶段新类型存在视为这些问题已解决，也不能为减少范围在真实请求链继续静默错误路由。

## 3. 逻辑模型与契约要求

### 3.1 三种资源

- `Profile`：稳定不透明 profileId、可编辑 displayName、当前不可变 ProfileVersionRef、deleted 状态。
  删除后 ID 不复活；重新创建同名 displayName 得到不同 ID。旧名称作为迁移映射，不作为版本 ID。
- `ProfileVersion`：profileId、revision、模型参数、完整 compat、凭据绑定引用。
  已发布版本不可覆盖；被历史选择引用的版本不可因删除列表行而回收。
- `HostConfigVersion` 与 `NewSessionDefaults`：分别拥有 revision；默认引用确切 ProfileVersionRef。
  发布 profile 新版本不暗改默认引用，修改 host 不重建所有 profiles。

revision/序号在 JSON 使用 canonical 十进制字符串，不用 JS bigint/浮点数；由 owner 分配，调用者不可指定新 revision。
初版 revision 从 "1" 起，缺失/删除不是 revision "0"；溢出具名拒绝，不回绕。
请求数字预算接受明确 u32 范围；秒数/计数如作为 JSON number 必须 <= 2^53-1，不饱和截断。

### 3.2 前端读与命令

保留 CFG-01 的接口目的，但版本化升级不能静默把当前无 revision 的 ProfileView 冒充新资源。
G0 固定最终方法表、响应 envelope 与兼容策略后再接线。建议目标名：

| 用例 | 输入要点 | 回包要点 |
|---|---|---|
| ListProfiles/GetProfile | ID、可选确切版本 | 脱敏资源及 revision，无 secret |
| CreateProfile | 客户端 requestId、完整合法初始配置 | 新 ID、revision、persisted 回执 |
| PublishProfile | profileId、expectedRevision、typed patch、requestId | 新不可变版本或 Conflict |
| DeleteProfile | profileId、expectedRevision、requestId | tombstone；默认引用禁止删除直至显式更换 |
| Get/UpdateDefaults | expectedRevision、确切 ProfileVersionRef | defaults revision，绝不隐式 apply |
| Get/UpdateHost | expectedRevision、host patch、requestId | 新版本、变更分类与 restartRequired 字段集 |
| Set/ClearCredential | 绑定资源、expectedRevision、显式动作 | opaque binding revision；无 token 回显 |

patch 三态：省略=Unchanged，`{op:"set",value:...}`=Set，`{op:"clear"}`=Clear。
Clear 仅适用于允许为空字段；null 不能冒充缺失，未知字段/非法 clear 拒绝，先验证整个命令再改状态。
写请求和含 secret 的结构不 derive 默认 Debug；不得在错误/日志中打印输入或 parser 原行。
Provider account 绑定只能引用受限端口确认的主体；没有可靠主体标识时返回 BindingUnavailable，不自行绑定“当前账号”。

### 3.3 错误与回执

必备 typed 错误：InvalidRequest、RevisionConflict、ProfileUnavailable、CredentialUnavailable、InvalidBinding、
PersistenceUnavailable、RepositoryClosed、ExternalModification、IdempotencyConflict、RestartRequired。
HTTP/SDK/TS 的 code 必须由同一契约测试锁定，不能只返回 action_failed 或让 UI 解析 Display 文案。
冲突回包包含当前有权查看的资源/版本；不暴露本机路径和秘密。

requestId 幂等：同 ID/同规范命令摘要返回原持久回执；同 ID/异摘要拒绝。
提交成功后响应丢失、重启后重试也不能发布第二个版本。原始 secret 不进回执；命令摘要不得暴露可离线猜测的 secret 哈希。
G0 必须明确如何安全绑定 secret-bearing 幂等输入；未定不能省略幂等或把明文落日志。
CAS 与幂等判定必须在同一个 writer 中针对实际持久状态执行，不“先查 revision 再排队”。

## 4. 实例与任务生命周期

- daemon/host 取得数据根独占权后构造 repository，注入 service、运行投影 resolver、文件导入器和管理 CLI。
- 一个 writer task/thread 持有权威状态、提交序列与文件句柄；队列有界，满时具名 Busy，不无限缓存。
- 同步老调用者可经有界 request/reply 适配；禁止持 registry/config Mutex 等待队列或 IO。
- shutdown：封闭准入→处理已接纳命令并结算回执→持久同步→join；不丢 JoinHandle，不新建 fallback runtime。
- 取消尚未接纳命令可以不执行；已接纳写命令必须给确定持久回执或明确未知，不把断开 HTTP 当回滚。
- read 可以获取不可变已提交快照；不能在快照指针与磁盘两处独立决定 revision。
- 通知携 resource ID/revision/变更种类；慢消费者重取当前资源，通知不参与授权和 commit 成功。
- host 安全收紧不能等闲置会话 reload 才生效；若本阶段无法闭合执行准入消费者，停止上线，不能只加通知。

## 5. 存储候选与 G0 必须解决的问题

候选布局（新格式，仅在 G0 批准后实施）：

```
<config-root>/configuration/
  manifest.json                  # 唯一发布点：schema、generation、资源索引、默认/host引用、回执索引
  profiles/<opaque-id>/<revision>.json
  host/<revision>.json
  defaults/<revision>.json
  receipts/<opaque-receipt-id>.json
```

secret 保留现有加密/权限存储机制，但新增不可变版本槽，通过受限端口引用；禁止直接覆盖正在被引用的 Main/profile 槽。
目录/文件角色和重建来源必须登记 docs/ARCHITECTURE.md；不是会话事实，也不写 session blob。

提交顺序：完整验证/CAS/幂等→新 secret 版本持久→不可变资源/回执文件持久→原子替换并持久 manifest→发布快照/通知。
只有 manifest 可达对象才已提交；预写孤儿不是成功，重启不自动发布孤儿；初版不做自动 GC。
Windows 替换使用实际支持覆盖的原子机制；文件同步、父目录/平台崩溃保证需诚实说明，不能把 std::fs::write 当 fsync。
manifest 替换失败不改内存 current，不发布 success；已发布引用必须可读；恢复缺引用为 CorruptRepository，封闭写入。

G0 必须在 ADR 明确并由 reviewer 批准：
1. 文件格式完整 schema、大小上限、路径校验、manifest 原子及崩溃保证。
2. secret 版本端口、set/clear/失败恢复及 secret-bearing 幂等安全方案。
3. 新旧进程/数据根排他、schema/version 拒绝、不允许旧二进制误写新根的方法。
4. 旧 config.toml 的 host/MCP/LSP/subagent/UI/secret 无损导入及未识别字段策略。
5. 同名 profile→稳定 ID 映射、缺失/歧义凭据如何阻止执行；不猜账号主体。
6. 旧运行态读取/写入 API 如何单向适配新 owner，以及所有实际调用者清单。
7. 跨仓消费版本矩阵、新旧服务方法名单/授权/错误码和移除旧写口时间。

### 5.1 CFG2-00 消费者核对与评审输入（2026-10-20）

以下是对当前工作树实现的消费者核对，不代表目标 owner、存储格式或兼容策略已获批准。

| 当前消费者/入口 | 可核对位置 | 对唯一 owner 的要求/退出项 |
|---|---|---|
| 服务读写与 profile 操作 | `qaqh-runtime/src/service.rs`：`config.load/save`、`config.set_permission_level`、`profile.apply/save_current/delete`、`session.set_profile`；服务表在 `qaqh-runtime/src/ringing/service_methods.rs`；客户端请求枚举在 `qaqh-client/src/endpoint.rs` | 全部全局写必须显式 adapter 进同一 repository 队列；旧输入没有 CAS/requestId，须由 G0 选择拒绝或限期 adapter，不能推测无冲突。`session.set_profile` 是会话选择，不得误并为全局默认写（CFG-03）。 |
| API key/profile key | `service.rs::profile.save_current` 先单独读主 key、写 profile secret，再 `Config::update`；删除先发布配置再尽力删 secret；`qaqh-config/src/secrets.rs::SecretStore` 现有 main/subagent/profile/MCP 槽 | secret 版本和配置引用须在 repository 单 writer 语义下提交；先写 secret 的孤儿只能是未发布对象。删除/clear 不能在新引用仍可见时破坏旧版本。profile secret 槽名仅接受受限字符，旧任意 profile 名可能无法映射，必须阻断迁移/执行而非继承 main key。 |
| 配置实例与通知 | `qaqh-config/src/config.rs::Config::update/save_with`；`qaqh-config/src/watch.rs::{authoritative,latest,reload_from_disk}`；`runtime/src/service.rs` MCP/LSP reload 与 config.toml mtime poller | 去除 static config/watch 权威及 mtime 作为第二写者；通知只能携 ID/revision/类别并触发重读。MCP/LSP reload、当前快照读取与 shutdown/join 都要注入实例 owner。 |
| 运行时读取/执行 | `runtime/src/agent/engine_session.rs` 的全局配置解析和 `Config::for_session`；`runtime/src/registry.rs` 两处直接 `Config::load` 取权限；`runtime/src/service/stats.rs::load_config`；`daemon/src/server.rs` 读取 idle unload；`runtime/src/service.rs` MCP/LSP `apply_config` | 应列入读适配器清单，传入不可变提交快照/解析结果；未知 profile、凭据缺失/无法解密不能退回默认端点或 main key。权限/主机执行收紧必须有即时拒绝消费者，不能等订阅会话 reload。 |
| 启动与离线写入 | `daemon/src/main.rs` 的 `mcp_import` 执行分支显式 `Config::load`、`mcp_import::import_servers`、`Config::save_with`；`qaqh-config/examples/cfgset.rs` 用于跨进程 secret 并发探针；公开 `Config::save/save_with/update` 可被其他 crate 调用 | 正式 daemon 的 CLI/import 必须通过同一 owner；离线迁移工具必须显式选定数据根、取得同一排他身份/停服许可。测试 fixtures 不构成生产 writer。需要完整引用搜索和逐调用点移除证据。 |
| 旧文件与资源读 | `ConfigStore`/`SecretStore` 默认使用 platform config path；`ConfigReader` 当前仅只读 `config.toml`；旧 `Config::load_from_paths_with` 对解析异常的行为与资源 reader 不同 | 单次停写备份、staging 导入、字段与凭据引用核验后切换；不得 online 双写，不得 query/migrate-on-read。未知字段策略及旧 reader/旧二进制拒绝新根必须由格式身份标记和启动准入共同保证。 |

#### G0 推荐案（2026-10-20 已批准；实施仍须遵守任务范围）

1. **存储**：保留 §5 manifest 唯一发布点与不可变资源/回执文件；manifest generation 是提交序列，外部 revision 用十进制字符串。候选 schema 必须有硬性文件/资源/整体大小上限、opaque ID 路径编码与拒绝 traversal 的规则、严格未知字段与重复键策略。临时文件需与目标同卷，文件数据及必要目录同步后才报告平台可证明的崩溃保证；Windows 替换语义需实际支持覆盖的原子 API/故障注入证据。只承诺 manifest 可达对象已提交，不承诺跨平台目录 fsync 超出平台能力。
2. **凭据**：增设受限 secret-version 端口，版本不可变并在资源引用发布前持久化；clear 以新引用/墓碑发布，不原地擦除正在被引用的槽。凭据失败时旧 manifest 与旧 secret 继续可读，新版本不成功；不可解密/主体不明均 fail closed。host/profile/provider-account 分开绑定，provider account 必须由受限 provider 端口给出稳定主体，不可自动选择当前账号。
3. **secret 幂等**：请求规范化摘要不得包含可离线猜测的原始 secret 哈希。推荐由 writer 在进程内以随机高熵 HMAC 密钥计算受域分隔的 secret-bearing 命令指纹；同一幂等记录中的随机 salt/key ID 与回执索引一起持久化，重启恢复验证能力。HMAC 密钥须由 OS 安全存储/DPAPI 等受限密钥端口保护，不能裸写 manifest 或回执。若平台密钥不可用，则拒绝该 secret-bearing 写入，不降级为明文摘要；G0 需确认 key 生命周期/备份恢复和记录保留期限。
4. **数据根排他**：repository 在解析/写入前取得数据根级 OS 独占锁，锁句柄由 owner 生命周期持有；仅进程内 mutex 不足。新格式含不可混淆的 format identity/schema version；旧二进制在触碰任何文件前必须因 marker 拒绝，且 marker 不得通过“缺失时自动初始化”绕过。第二 host/CLI 同根失败为具名 AlreadyOwned/Busy。不同显式根可并行。
5. **迁移**：只提供停服离线 staging importer。逐字段映射并核对 model/wire/compat、effort/budget、host exec/permission/session、MCP/LSP/subagent、UI 偏好隔离、main/subagent/profile/MCP secret 引用和加密可读性；源/备份只读保留。未知字段默认拒绝发布并给出不含值/路径/secret 的字段名清单；只有经 G0 明确允许的无语义扩展字段才可隔离保存，不能静默丢弃。重复/非法 profile 名或 secret 绑定歧义阻断切换。
6. **读写适配与运行消费者**：daemon composition root 创建单一 owner 并注入 runtime/service、Config resolver、MCP/LSP reload、stats、registry 权限及 daemon idle 消费者。旧服务 adapter 不得自行读写文件；其授权保持 admin-only。MCP import 归 owner 队列，离线 fixture API 与生产写 API 分离。当前读消费者全部逐项测试 owner 替换，权限收紧以执行准入读取最新已提交快照，而非依赖异步通知完成。
7. **旧客户端/版本兼容**：推荐在新根启用时对所有无 expectedRevision/requestId 的旧 config/profile 全局写方法返回稳定的 `configuration_upgrade_required`（不执行、不隐式取当前 revision），保留只读旧契约直到版本矩阵允许删除；不得静默“最后写者胜”。新 writer 不响应旧方法成功回执。旧二进制遇新根须启动前拒绝；回退只能停写、备份新根、恢复切换前旧根，或经显式导出/向后迁移保留新写入，禁止旧二进制打开新根。删除旧写 adapter 的最迟版本仍按 §6 v3.0。

#### G0 批准记录与实施探针（封闭问题已全部按推荐案批准）

- **G0-1 secret 幂等密钥：已批准。** 使用受 OS 安全存储保护的高熵 HMAC 指纹；平台密钥不可用时拒绝 secret 写入，不降级为可离线猜测摘要。
- **G0-2 文件崩溃承诺：已批准。** 同卷临时文件、文件 flush/sync、原子替换及平台支持时同步目录；具体平台/文件系统保证须由实现探针证明，不能夸大 Windows 断电保证。
- **G0-3 数据根锁与旧二进制：已批准。** OS 独占锁和 format identity；同根冲突拒绝，新格式不得被旧二进制触碰。
- **G0-4 未知旧配置字段：已批准。** 默认阻断迁移，仅 allowlist 的无语义扩展可隔离保留，不静默丢字段。
- **G0-5 profile/凭据歧义：已批准。** 重复/非法名、secret 不可读、主体不明等 fail closed，不继承 main key。
- **G0-6 旧写客户端：已批准。** 新根启用后旧全局写方法稳定拒绝升级，旧只读暂留；旧写 adapter 最迟 v3.0 移除，旧二进制拒绝新根。
- **G0-7 consumer closure：已批准。** runtime/daemon/MCP/LSP/CLI 注入唯一 owner；权限在每次执行准入读取最新已提交状态。

CFG2-00 的 G0 阻塞解除；按 §7 串行推进。该批准不授权提前注册写路由或实施存储；CFG2-02/03/04 仍分别受其前置任务约束。

## 6. 切换与兼容合同

一次数据根切换，不在线双写 config.toml 和新 manifest：
停写/收割配置任务→备份原配置和凭据→staging 离线导入→字段/secret引用/完整性核对→发布格式身份→启动新 owner→探针。
原文件保留备份，读取不 migrate-on-read；迁移失败不得覆盖源文件或自动空配置启动。
旧格式已经被 CFG-01 reader 拒绝的情况必须经宿主迁移处理，不能放宽读边界静默补值。

旧服务口可暂留：唯一 owner 为新 repository，方向是旧请求→显式 adapter→同一队列；只返回原有契约。
无 expectedRevision 的旧写口不得绕开 CAS：G0 选择“停止支持旧写客户端/具名升级拒绝”或批准有明确风险边界的过渡。
不能由 worker 擅自把新旧客户端写入都当作无冲突；最迟 v3.0 删除 adapter。
`Config::update/save/save_with` 在生产服务态不再是第二写口；离线 fixture/迁移工具应独立显式 API，不用隐藏全局模式开关。
新写后禁止旧二进制直接打开新根；回退点和保留新写入的策略必须有演练证据。

## 7. 任务拆分（便宜模型一次只领一个）

| ID | 前置 | 交付与允许文件 | 删除/完成标准 |
|---|---|---|---|
| CFG2-00 | handoff | 完成§5七项 ADR/迁移/消费者表；只改本spec、配置ADR、architecture | reviewer 明确批准；不写生产实现 |
| CFG2-01 | CFG2-00草案 | config-api 新资源契约/命令/错误/三态patch、共享JSON向量、TS测试 | 纯serde依赖；不注册方法；未知字段/null/别名/数字/脱敏向量通过 |
| CFG2-02 | G0批准、01 | config/repository、storage、recovery与隔离IO故障测试 | manifest唯一提交点；重启/CAS/幂等/secret失败证明；无生产接线 |
| CFG2-03 | 02 | config/migrate、离线入口、数据格式拒绝/回退探针 | 全字段与引用保真；源数据不改；失败停留旧根；不在线双写 |
| CFG2-04 | 02、03 | daemon/service装配、runtime config_service、watch消费者、显式resolver | 全部写调用接同一owner；不持锁等待；shutdown+join；不漏host安全收紧 |
| CFG2-05 | 04 | client typed读写、daemon方法表/授权/错误、TS向量 | 真实HTTP admin拒绝/零会话管理/CAS冲突/回执恢复，且无token泄漏 |
| CFG2-06 | 03–05、跨仓批准 | 删除旧writer/补偿、旧接口退出、架构文档 | 新旧版本拒绝矩阵；数据切换/回退演练；扫描旧写口无生产调用 |

CFG2-02/03 不可在两模型并行修改同一存储格式；04/05也须固定契约提交后串行或按文件明确分工。
不用 task 数/代码行数宣称完成；只交类型或空 crate 不算02/04完成。

## 8. 必须覆盖的接缝用例

- R01：编辑非默认B不切默认A、不覆盖A/host；仅显式UpdateDefaults改变默认引用。
- R02：同revision竞争，恰好一个成功；另一个Conflict；重启后结果相同。
- R03：requestId重试无新版本；异摘要拒绝；响应丢失/重启后仍一致。
- R04：secret失败、资源写失败、manifest失败的每个窗口，旧版本仍完整可读，不发成功通知。
- R05：发布后引用缺失/损坏封闭写入，不从旧config.toml补成功。
- R06：删除后迟到save不复活ID；当前默认/被保留版本引用受保护。
- R07：未知profile/key不可回落另一个endpoint；账号绑定缺证据明确不可执行。
- R08：nullable字段Unchanged/Set/Clear各自往返；未知字段/非法clear/越界在提交前拒绝。
- R09：同进程两个显式不同根互不污染；同根第二host被拒，非static锁伪证明。
- R10：shutdown/队列满/请求断开不吞写结论；所有writer任务可join。
- R11：新权限收紧即时阻止后续执行；MCP/LSP/模型改动只触及对应消费者。
- R12：旧配置导入保留compat/cache/effort/预算/MCP/LSP/子任务参数；UI偏好不丢失但不变成host设置。
- R13：API/日志/Debug/导入失败无secret；parser源行不透传；真实鉴权HTTP拒绝view/interact写配置。
- R14：native类型和TS共享向量；JSON数字/序号无精度丢失；未实现路由不提前宣布可用。

## 9. 验证与提交报告

每任务运行自己的unit/接缝与修改文件rustfmt、git diff --check。重要命令：

```
cargo test -p qaqh-config-api --features ts
cargo test -p qaqh-config -- --test-threads=1
cargo test -p qaqh-runtime --lib config_service
cargo test -p qaqh-runtime --lib configuration_resource_queries
cargo test -p qaqh-daemon config_resource_failures
cargo test -p qaqh-client --test v2_public_api
cargo test --workspace --no-run
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

TS_RS_EXPORT_DIR 显式设为本工作树 target/config-contract-ts，不能污染正式前端生成目录。
全量workspace测试运行、外部账号/模型探针、agent启动类测试先获所有者授权；当前禁止子代理，不能为验收绕过。
如无法运行AGENTS-x要求的完整门禁，报告“未验证/阻塞”，不得宣称任务组全部完成。

每次交付只需：任务ID、修改范围、真实消费者/删除项、测试命令及结果、未验证项、下一任务/停止点。
不顺手修范围外格式/lint；发现已有门禁问题记录具体位置和命令，禁止删测试、allow lint或改基线制造通过。
