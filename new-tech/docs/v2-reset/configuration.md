# config/config-api：资源契约与当前施工边界

CFG-01 起点：backend `80ddb3c`，RC2，33 workspace members。所有者授权优先前端适配与后端服务管理，
必要时整体替换旧结构，不新增独立配置权威。实施依据见 [ADR](../../../docs/adr/2026-10-20-config-resources.md)。
后续施工按 [CFG-02 spec](../../../docs/spec-config-resources.md)；接手现状与可直接派发的任务见
[handoff](../../../docs/handoff/2026-10-20-config-resources.md)。spec 的 G0 未批准前不启动生产写入/格式切换。

## 1. 状态与唯一所有者

- config-api：无 IO 的 Wire/CredentialSource/EndpointCompat 与配置 DTO；默认仅依赖 serde。
- config：资源文件读取、具名错误。ConfigReader 显式路径，不解密、不初始化、不迁移、不读全局 watch。
- runtime：资源查询用例及错误映射，无配置 writer。
- daemon：现行 service 路由 admin 准入、HTTP 状态。未连接业务会话也能查询；仍需有效客户端连接身份。
- client：config_catalog/config_profile/config_host 的具名方法和固定响应类型。

types/provider 的定义及算法测试已迁至 config-api；旧类型路径仅单向导出同一类型，最迟 v3.0 随 types 收敛退出。
现行 config.load/save/profile.* 尚未迁移，不能因读模型落地宣称唯一实例 repository 或写入 CAS 已完成。

## 2. 前端消费

| 方法 | 输入 | 结果 | 用途 |
|---|---|---|---|
| config.catalog | `{}` | ConfigCatalog | 一次获取按 profileId 排序的完整目录与 defaultProfileId |
| config.profile | `{profileId}` | ProfileView | 读取任意 profile，不改变默认；默认损坏仍可查询已知条目用于修复 |
| config.host | `{}` | HostConfigView | 主机权限/执行/会话/集成声明，不返回客户端 theme/font/lang |

ProfileView 包括 model/baseUrl/wire/maxTokens/contextLength/reasoningEffort/compat 与 credential declaration。
credential kind 为 profile_key/host_key/provider_account；只解释旧格式的绑定来源，不证明密钥存在、可解密或远端已登录。
不返回 token、secret 路径、掩码魔法串、MCP 的 argv/env/header 值。不得把声明当成 actor 实际 applied 状态或服务健康。

DTO 外层 camelCase。compat 与持久协议值共用现行 snake_case 和枚举拼写，由 ts feature 导出，客户端不手抄另一份。
JSON 整数字段导出为 TypeScript number；超过 2^53-1 的主机计数/秒数与 retry 秒数拒绝，避免前端精度静默丢失。
类型导出测试写 target/config-contract-ts；正式桌面生成物仍由桌面仓执行 ts-export 和 typecheck。

## 3. 失败与兼容

三个新查询 admin-only；旧读写方法和回包不变。不新增 HTTP 路径、磁盘文件或消息事实。
未知/多余请求字段拒绝；错误码可用于 UI 动作，不返回含原文的 TOML parser 错误。

| code | HTTP |
|---|---|
| invalid_config_request | 400 |
| profile_not_found | 404 |
| configuration_unavailable | 503 |
| invalid_configuration / invalid_default_profile / config_migration_required | 409 |

旧格式只提示宿主迁移，不在 query 中写回。没有 revision/CAS 的读取结果不能作为并发编辑成功依据。

## 4. 后续重写，而非扩展旧 save

CFG-02：实例 repository + 单一 writer queue；指定资源 CRUD、不可变 revision、CAS、默认独立命令；
secret 版本先持久，再原子发布配置引用，失败保留原配置。旧写口迁移到同一 owner 后关闭，最迟 v3.0 删除。
不再用全局 apply→save 编辑目标，不覆盖当前 secret 后假装可回滚。

CFG-03：actor 接纳 selection facts，pending/applied 与下一 turn 边界；完整 request snapshot 供 main/compact/title/child，
重启按确切版本恢复。未完成前不称为持久多配置隔离；缺证据返回 NeedsConfiguration，不猜测恢复。

多端 UI 的实际接入、真实账号与远端探针分别验收；本仓的 DTO/类型测试不能证明这些已经交付。

## 5. CFG-01 验证边界

本工作树已执行：config-api 含 ts feature 的测试与严格 Clippy、config 全量测试、资源分发/参数拒绝测试、
daemon 错误映射测试、服务表 admin 准入声明测试、client 公共类型测试；`cargo test --workspace --no-run` 通过。
修改文件 rustfmt 与 git diff --check 通过。测试使用隔离临时配置，不发起模型/账号远端请求。

全量 `cargo fmt --all -- --check` 未通过：范围外已有格式差异；workspace 严格 Clippy 未通过，
包括未修改 import 的 lint；config 严格 Clippy 被未修改 config.rs:865 的 collapsible_if 阻断。
未执行 workspace 全量测试运行（避免触发 agent/账号等非本切片路径）、桌面 ts-check/构建、真实鉴权 HTTP 往返或账号探针。
服务表/错误映射测试不替代真实多端接入。以上是当前验证证据，不表示 CFG-02/03 或整个配置重构完成。
