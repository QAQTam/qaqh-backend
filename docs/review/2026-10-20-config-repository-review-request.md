# Claude 审阅请求：CFG2-02 隔离配置 Repository 草案

**审阅目标**：请对当前工作树中的 CFG2-02 隔离实现做设计与正确性审阅，重点判断它能否满足已批准的 G0 和 `docs/spec-config-resources.md` 的存储/恢复要求。请把未实现项、失败验证和不确定的平台保证与已验证实现分开陈述。

**重要状态**：这是未接入 daemon/runtime 的实现草案，不是生产配置权威，也不是 CFG2-02 已完成。请勿据此批准生产切换、生产写路由或迁移。当前生产读写仍走 `config.toml`、`secrets.toml`、旧 `Config::update/save_with` 及旧 service 方法。当前任务边界不包含生产装配。

## 给 Claude 的操作边界

- 只读审阅；请勿编辑、格式化、删除、移动或生成本仓库文件。
- 不运行真实账号、provider、收费模型或网络请求；不访问/修改用户配置和生产数据根。
- 不启动 agent，不调用子代理。
- 不自行重写实现或扩大到 CFG2-03/04。需要改设计时，给出具名 finding 和建议，等待负责人裁决。
- 不把上一轮曾通过但之后又改过的测试结果当作当前状态；应以本文列出的最新可核证状态为准。

## 已批准的设计基线

用户已对 spec §5.1 的 G0-1 至 G0-7 全部批准推荐案，摘要见 `docs/adr/2026-10-20-config-resources.md:39-43` 和 `docs/spec-config-resources.md:164-184`。相关重点：

1. 每数据根唯一 owner；manifest 是唯一发布点；资源版本不可变。
2. secret 版本经受限端口先持久再发布配置引用；平台安全密钥不可用则拒写。
3. secret-bearing 幂等使用 OS 安全存储保护的高熵 HMAC/key-id；回执中不存明文或可离线猜测的 secret 哈希，密钥不可用时拒写。
4. OS 独占锁和 format identity；未知旧格式/字段默认拒绝，不在线双写。
5. 旧格式仅停服 staging 迁移；凭据歧义 fail closed，不自动继承 main key。
6. 旧无 CAS/requestId 全局写入稳定拒绝；所有运行消费者最终注入唯一 owner；执行准入读取最新已提交状态。
7. 文件系统/Windows 的断电保证必须由实际探针证明，不得夸大。

## 当前草案位置与实现形状

主要代码：`crates/qaqh-config/src/repository.rs`；模块目前仅从 `crates/qaqh-config/src/lib.rs:6` 导出，尚未接 daemon/runtime。候选布局为 `<data-root>/configuration/manifest.json` 加不可变 profile/host/defaults 版本资源和回执。manifest 保存 generation、profile/defaults/host/credential 索引、幂等回执/key-id、资源 SHA-256。repository 尝试在每次操作前比对 manifest 快照并校验已发布资源摘要；持 `.owner.lock` OS 锁；资源版本采用 no-replace 发布，manifest 原子替换作为提交点。

Secret 和命令指纹通过 `SecretVersionStore`、`CommandFingerprinter` trait 注入。当前只有测试替身；没有生产 OS 安全密钥 provider，也没有 production immutable secret backend。Windows `MoveFileExW` 与 Unix 目录同步代码尚未做断电/文件系统故障探针。不要把这些 trait 误认为已经实现凭据安全。

当前测试范围是显式 tempfile 根下的隔离测试。已有用例覆盖：同根排他/不同根隔离、重启幂等、requestId 异摘要冲突、manifest 修改和提交失败、引用缺失/摘要不符、资源孤儿不覆盖、credential CAS/clear tombstone、历史 profile 版本默认引用、HMAC key rotation 重放、receipt schema 损坏、密钥端口失败 fail closed。请逐项检查它们是否真的证明相应不变量，而不是只看测试名。

## 最新验证事实与当前阻塞

- `cargo check -p qaqh-config` 在加入严格 JSON duplicate-key scanner **之前**通过；随后实现状态又发生变化，不能视为当前 HEAD 的通过证据。
- `cargo test -p qaqh-config -- --test-threads=1` 最近一次完整通过发生在 duplicate-key scanner 加入前；之后当前单测状态发生变化，不能据此称当前完整测试通过。
- `cargo test -p qaqh-config repository::tests -- --test-threads=1` 最近一次覆盖 16 个 repository 用例时通过，但发生在 duplicate-key scanner 的 visitor 改动之前。
- **当前明确失败**：`repository::tests::strict_json_parser_rejects_duplicate_keys_at_any_depth`。重复嵌套 JSON key `{"outer":{"value":1,"value":2}}` 仍被 parser 接受。相关实现为 `repository.rs:1020-1113`，失败测试在 `repository.rs:1855-1861`。这意味着 G0 已批准的“严格 schema/重复键拒绝”目前未满足；不要宣称构建/测试已全绿。
- 最新完整 `cargo clippy -p qaqh-config --all-targets -- -D warnings` 被依赖链中未修改的 `sbx-win` 12 个 lint 与 `qaqh-types/src/config.rs` 的 lint 阻断。`cargo clippy -p qaqh-config --lib --no-deps` 能到本 crate，但报告未修改的 `crates/qaqh-config/src/config.rs:865` `collapsible_if`。这些范围外问题没有被本任务修改。
- 此后 `cargo check -p qaqh-config` 曾再度通过一次，但紧接着 duplicate-key 测试仍失败。因此当前 blocker 明确是 scanner 语义，不是已证明的端到端验收。
- `rustfmt --edition 2024 --check crates/qaqh-config/src/repository.rs` 与相关文件 `git diff --check` 最近通过（scanner 最新变更后仍需复核）。

## 希望重点审查的问题

请优先逐条回答并引用 `file:line`：

### P0：提交、并发与恢复

1. 外部非协作写者能否在 `ensure_current`/`commit_manifest` 检查后、manifest replace 前插入修改，导致原子 replace 覆盖其更新？OS lock 只能约束遵守同一锁协议的 writer；若不能原子 CAS manifest，请说明可证明边界或要求何种机制。
2. 资源 no-replace 写成功、manifest 发布失败后，孤儿在重试、profile revision、host/default revision、defaults 引用下是否安全？所有窗口是否只会留下不可达对象而不产生“成功但不可恢复”？
3. 同卷资源 no-replace、manifest replace、父目录同步的 POSIX/Windows 语义是否准确？hard-link 不支持的文件系统、Windows 目标已存在错误分类、目录同步失败和断电后的可见状态如何处理？哪些结论必须降为未验证？
4. `ensure_current` 对所有读/写入口的覆盖是否完整？`generation()`、`close()` 等是否需要状态校验或明确定义例外？持有锁句柄的生命周期和 close 后解锁行为是否安全？
5. manifest/resource hash、schema marker、size/path 限制、严格 JSON duplicate key、未知字段 deny 策略是否覆盖完整？当前 scanner 已知重复 key 测试失败，请指出正确做法/边界，不能以 serde 普通解析通过替代。

### P0：幂等与秘密

6. 指纹是否真正使用 HMAC 和受 OS 保护密钥？`key_id` 是否跨重启持久、可解析旧密钥、密钥轮换/退休策略是否和 receipt 保留期一致？同 requestId 同命令、异命令、响应丢失、manifest commit 后崩溃如何证明？
7. `SetCredentialRequest` 中 secret 的序列化、错误、日志、panic、Debug 是否可能泄漏？fingerprint 输入是规范且域分隔的吗？是否存在明文/低熵哈希写入持久文件的路径？
8. `SecretVersionStore` 是否实现不可变槽、先落盘/fsync、权限、版本读取验证、旧版本保留？Clear 如何表达墓碑并保持历史 ProfileVersion 引用可解析？provider account 稳定主体绑定是否有充分证据？
9. repository 和 secret backend 谁权威分配 credential revision？当前设计把 revision 交给 backend 并校验返回 binding；检查失败窗口、重复 set、set-clear-set、不同 requestId 和并发是否会重用/分叉 revision。

### P1：数据模型与兼容

10. 删除 profile 后默认历史版本和引用保护是否正确？`ProfileIndex.deleted`、revision、manifest 索引、历史资源回收规则是否一致；当前是否存在删除后 ID 重用或 profile name 映射歧义？
11. defaults 精确引用旧 ProfileVersionRef；发布新 profile 不改 defaults；删除默认引用目标如何拒绝。历史版本检验是否确实按文件摘要验证，而非只看索引。
12. `HostConfigView` 是否覆盖 spec 中所有 host 设置；当前结构对 MCP/LSP/subagent/权限执行消费者是完整数据源还是仅部分 DTO？不要把 incomplete host patch 冒充完整适配。
13. 命令错误映射、revision overflow、requestId 规范、receipt 容量硬限制是否符合 wire contract；receipt 4096 满后永久拒写是否可接受、是否需有明确迁移/GC 生命周期（不得 TTL 静默丢幂等终态）。
14. 读取/serde Debug 错误是否会包含 source line、secret 或本机路径；错误类型是否保留足够上下文又不泄露？

### P1：实现范围与安全边界

15. `Root::open` 对“根不存在/空目录/已存在无 marker/并发首启”的判断是否避免旧二进制误初始化新根？不同 host 同根排他和不同根隔离是否有正确操作系统锁语义。
16. 生产装配前还缺哪些必须交付项？至少考虑 OS HMAC provider、secret backend、Windows crash/replace probe、资源/manifest 完整故障矩阵、queue/busy/shutdown 生命周期、离线迁移和真实 consumers closure。请区分 CFG2-02 可补项目与后续 CFG2-03/04 项，避免建议越任务接线。

## Claude 输出格式

1. **Findings 优先**，按 Critical/High/Medium/Low 排序。每条必须有文件行号、触发条件、实际影响、最小修正/测试建议；没有证据就标“待验证”，不要推断已发生。
2. 给出“阻塞生产装配”清单和“纯隔离 CFG2-02 可继续”清单。
3. 单独列出本次验证命令应补的测试矩阵，尤其重复 JSON key、manifest 外部写竞争、崩溃窗口、HMAC 轮换、secret store failure、Windows 文件语义。
4. 不要给出“已安全/可上线”泛化结论。当前生产端口未实现、重复键测试失败、clippy 被既有依赖 lint 阻断，这些是已知状态。
5. 只回复审阅报告；不要修改本仓库。

## 本仓当前任务停止边界

当前仅审阅/修正 `qaqh-config::repository` 隔离存储核心与测试；不注册写路由、不装配生产 writer、不迁移用户数据、不改 production config、不调用真实 provider。CFG2-03 离线迁移、CFG2-04 生产 owner 消费者装配、CFG2-05 路由/客户端、CFG2-06 旧路径退出均未开始。G0 已批准不等于这些后续任务已授权实现。

## 相关文件

- `docs/spec-config-resources.md`：CFG2 目标、G0、验收任务 §7、接缝 R01-R14 §8。
- `docs/adr/2026-10-20-config-resources.md`：G0 批准与配置契约背景。
- `docs/handoff/2026-10-20-config-resources.md`：未提交基线注意事项、CFG-01 验证和交接边界。
- `docs/ARCHITECTURE.md:167`：当前生产权威与隔离 repository 目标表。
- `crates/qaqh-config/src/repository.rs`：本次主要审阅对象。
- `crates/qaqh-config/src/secrets.rs`：现有旧 `SecretStore`；不可直接将其等同为版本化生产 secret backend。
- `crates/qaqh-config-api/src/commands.rs`：纯 wire 请求/回执/错误契约。
