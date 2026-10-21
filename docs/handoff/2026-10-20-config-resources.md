# config/config-api 交接：CFG-01 → CFG-02

日期：2026-10-20。接收对象：用户指派的施工模型；**禁止调用任何子代理**。
用户方向：为了前端更好适配、后端更好管理服务，必要时砸破重写，而不是缝补旧结构。
执行规格：[spec-config-resources](../spec-config-resources.md)。本文件交接现状，不是第二份派工规格。

## 1. 接手先做的事

1. 读 `AGENTS-x.md`、本文件、spec §0/§7、现行 `new-tech/docs/v2-reset/configuration.md` 和配置 ADR。
2. 运行 `git status --short`、`git rev-parse HEAD`，确认当前分支及前任修改都在。
3. 本次交接核对 HEAD 为 `80ddb3c785a0af4dfe217169119c1940bb53f7eb`，分支工作先前在 `codex/v2.1-refactor`。
   Cargo 为 RC2、33 members。**CFG-01 尚未提交，包含未跟踪新文件。只按 HEAD 建分支会丢掉这一批。**
4. 用户/reviewer 可以先检查并提交 CFG-01；施工模型不得擅自提交全部脏文件、reset、clean、restore 到 HEAD。
   若交给另一个checkout，必须由用户迁移完整工作树补丁（含untracked），不能只复制 git diff --stat 展示的18文件。
5. 一次只领一个任务。推荐先派 `CFG2-00`；未过 G0 不允许生产存储/写口施工。

禁止真实登录/登出/refresh/verify、收费模型请求、spawn/唤醒agent、修改用户配置；探针使用临时显式路径。
网络安装/构建按当前工具权限申请；不能因为没有批准就调用exec逃避文件工具或私自启动外部任务。

## 2. 已落地但未提交的 CFG-01

目的：前端能直接看见非活跃 profile 完整配置，后端查询不通过“先apply再读”的有副作用路径。
这不是完整repository、持久多配置隔离或新writer。

| 文件 | 内容/所有者 |
|---|---|
| `crates/qaqh-config-api/src/provider.rs` | 原types/provider协议值类型及4个算法测试迁入；新增TS导出 |
| `crates/qaqh-types/src/provider.rs` | 只有单向类型导出；不是误删436行，需要保留新文件一起迁移 |
| `crates/qaqh-config-api/src/lib.rs` | 导出资源契约，canonical_wire共用Wire::parse，纠正MergePatch/CAS误述 |
| `crates/qaqh-config-api/src/resources.rs` | 三个只读方法常量、ProfileView/Catalog/HostView、凭据声明和稳定错误码 |
| `crates/qaqh-config/src/resources.rs` | ConfigReader显式路径；不创建、不迁移、不解密、不读watch |
| `crates/qaqh-runtime/src/config_service.rs` | typed查询错误、请求严格校验、资源分发测试 |
| `crates/qaqh-runtime/src/service.rs` | 三个查询接线；原config.save日志收短以不扩大>1500行文件 |
| `crates/qaqh-runtime/src/ringing/service_methods.rs` | admin-only、无需业务session的服务表条目及测试 |
| `crates/qaqh-daemon/src/axum_server/axum_impl/service_api.rs` | 复用现有HTTP路由，typed错误映射与测试 |
| `crates/qaqh-client/src/config.rs` | config_catalog/config_profile/config_host固定响应方法 |
| `crates/qaqh-client/src/lib.rs` | 原生壳可命名资源类型，直接消费config-api的Wire/CredentialSource |
| `crates/qaqh-config/src/dto.rs` | 删除已无意义的“两个Wire实现互相对拍”镜像测试；真实roundtrip测试仍在 |
| `crates/qaqh-config-api/tests/resource_contract.rs` | 参数闭表、残缺拒绝、错误码与TS number测试 |
| `crates/qaqh-config/tests/config_resources.rs` | 六个资源读取/脱敏/失败/无副作用/精度接缝测试 |
| `crates/qaqh-client/tests/v2_public_api.rs` | 资源类型和方法的公共API可命名测试 |

对应Cargo.toml和Cargo.lock已调整。新增正常依赖：types/client/daemon→config-api；config-api默认非可选依赖仍只有serde。
`qaqh-types/ts` 转发 `qaqh-config-api/ts`。不要让config-api反向依赖types/runtime/provider实现。

文档：`docs/adr/2026-10-20-config-resources.md`、`docs/ARCHITECTURE.md`、
`new-tech/docs/{README,ARCHITECTURE}.md`、`new-tech/docs/v2-reset/configuration.md`。
本次新增的spec/handoff也是未提交文件。

## 3. 当前外部契约（不要猜）

- `POST /ringing/v2/service/config.catalog`，输入 `{}`，返回完整排序目录与defaultProfileId。
- `.../config.profile`，输入 `{ "profileId": "name" }`，返回完整条目。未知/多余字段拒绝。
- `.../config.host`，输入 `{}`，不含theme/font/lang和MCP argv/env/header值。
- 全部admin-only；无需attach业务session，但仍需现行client connection/lease身份。
- credential.kind 为profile_key/host_key/provider_account：**只是本地声明，不证明secret存在、可解密或账号有效**。
- 外层camelCase；compat沿共享EndpointCompat的snake_case与既有枚举拼写，不手工加第二套映射。
- integer秒数/计数的TS是number，reader拒绝>2^53-1；未来revision用字符串。
- error code：invalid_config_request=400，profile_not_found=404，configuration_unavailable=503，
  invalid_configuration/invalid_default_profile/config_migration_required=409。
- 当前profileId就是旧文件profile名称，不是假装已有稳定ID或revision。
- 缺文件/坏文件不回默认、不创建模板；旧格式返回migration_required，不查询时写回。

原config.load/save/profile.*没有被替换。新读取不代表旧写入已经安全，也不能用资源view冒充actor applied。
默认坏了catalog会报错；已知profile详情仍可读用于修复。列表修复体验可在CFG-02契约评审提出，不私自放宽默认验证。

## 4. 当前没有做的工作

- Config::update的static锁与Config::save_with重建active profile仍在。
- Config::apply_profile、SessionEngine::apply_config遗漏credential的静态缺口仍在。
- for_session缺profile回active、命名key/主key继承、子任务重读global仍在。
- session.set_profile的persist_profile忽略写错误仍在；不是本轮新查询已修复。
- 没有不可变profile revision、CAS、独立defaults命令、secret版本、持久回执、repository shutdown。
- 没有前端仓真正接线、TS typecheck/build、多语言客户端迁移或真实HTTP鉴权全链验收。

不要顺手逐项补字段后宣布重构；后续按spec明确的writer/契约切片解决。
账号主体与请求快照需要provider/CFG-03协调，证据不足就具名失败，不帮用户“自动选当前账号”。

## 5. Git与文档陷阱

独立分支有 `3aa7244` 文档校准提交，**不在当前HEAD**。其configuration/preservation内容不是当前工作树直接基线。
当前已创建新的现行configuration.md；禁止直接cherry-pick该提交覆盖它或把旧文档中的37167d7当最新产品。
该提交的配置设计可供reviewer核对，但施工只按本spec和批准ADR。

new-tech旧入口还引用不存在的preservation.md、tools/v2-architecture/audit.py、历史baseline.json；
不凭空造历史baseline，不为了让链接全绿引入整个旧设计。此为已知文档缺口，不是CFG-02存储失败。
new-tech/ARCHITECTURE仍有again历史表；顶部当前配置切片段才说明本工作树，不要把alpha10/32包当当前实现。

格式化曾碰到四个无关文件，已用spy undo撤回；不能再次全包format把它们加回来。
不要将types/provider体积骤缩判为事故回滚；定义和测试在config-api/provider，workspace测试目标编译已经检验过。

## 6. 已执行验证与局限

以下是上一轮实际工具结果，不表示接手后新增代码自动通过：

| 命令/检查 | 结果 |
|---|---|
| `cargo test -p qaqh-config-api --features ts` | 37个unit/导出+4个integration通过 |
| `cargo test -p qaqh-config -- --test-threads=1` | 全量通过，含6个resource用例 |
| `cargo test -p qaqh-runtime --lib config_service` | 2个通过 |
| `cargo test -p qaqh-runtime --lib configuration_resource_queries` | 1个通过 |
| `cargo test -p qaqh-daemon config_resource_failures` | 错误映射1个通过 |
| `cargo test -p qaqh-client --test v2_public_api` | 5个通过 |
| `cargo test --workspace --no-run` | 全部测试目标编译通过，未执行全量用例 |
| `cargo clippy -p qaqh-config-api --all-targets --features ts -- -D warnings` | 通过 |
| 修改文件rustfmt，`git diff --check` | 通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 失败：包括未修改qaqh-import的12个lint |
| config/config-api --no-deps严格clippy | 被config.rs:865既有collapsible_if阻断 |
| `cargo fmt --all -- --check` | 失败：已有client/secrets/daemon/sbx等范围外格式差异 |

测试TS产物在 `target/config-contract-ts/qaqh/`，不是正式前端提交物。
PowerShell设置：`$env:TS_RS_EXPORT_DIR = Join-Path (Get-Location) 'target/config-contract-ts'`。
曾修复的本轮失败：daemon缺Json导入、ts-rs 12的TS::decl需要Config参数；最终相关测试已绿，不用再重复调查。
真实账号/收费模型未调用。workspace全量测试运行未执行，防止agent/远端路径；不得把--no-run说成全量测试通过。

## 7. 直接可发给施工模型的任务

### 首个任务：CFG2-00（推荐）

> 在当前工作树读取AGENTS-x、配置handoff和spec。禁止子代理、真实账号、agent启动及生产配置修改。
> 只执行CFG2-00，核对全部配置读写消费者，补齐spec §5七项设计评审输入，更新配置ADR/架构表。
> 明确secret版本/安全幂等/数据根排他/旧写客户端处理/切换回退。不得写生产存储实现、注册新写路由或选择第二writer。
> 遇到需要裁决的地方提出封闭问题和推荐方案，等待G0批准。交付任务ID、文件、证据和下一停止点。

### G0后任务模板

> 只执行 `CFG2-XX`，以前置任务批准提交为基线。允许文件和验收见spec §7–9。
> 不跨任务补洞，不改已有脏文件，不清理无关lint/格式，不新增allow来让门禁变绿。
> 测试用显式临时根；写路径必须有真实消费者、失败/重启证据和旧路径删除项。
> 回报实际命令/结果和未验证范围；没有跑的不说通过。需要改变spec或权限边界先停下来。

便宜模型不宜同时承担存储格式、secret事务、API和actor全部重构；按任务串行交付，由reviewer审查关键提交。
