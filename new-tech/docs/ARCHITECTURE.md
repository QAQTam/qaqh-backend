# 实现状态与架构目标

校准：2026-10-20。现行设计入口：[v2-reset](v2-reset/README.md)；发布规则见 [rollout](v2-reset/rollout.md)。
**从当前业务提交渐进迁移；again 是历史设计来源；以下目标不表示已实现。**

## 1. 基线与状态

| 对象 | 核对结果 | 使用方式 |
|---|---|---|
| 历史设计来源 | `v2.0-again` / `13d1e59`，原记录 alpha10 / 30 members | 不作为当前源码或重新开工基线，本轮未重新核验该历史 checkout |
| 当前 backend 源码基线 | `37167d74b96fdef0700f5ca886475fb59e76d238`，Cargo `2.0.0-RC2` / 32 members | 本文档工作树从该提交创建；与业务 checkout 的未提交修改分开 |
| 当前客户端 | desktop `6b86594`、TUI `f7e47af`、Android `cf63363`、Harmony `901b9d0` | 完整提交和消费路径见保留矩阵；源码存在不表示本轮构建通过 |
| 并行施工 | 业务 checkout 有 transcript-import 草案、未跟踪 crate 和 Cargo/架构文件修改 | 未纳入已提交基线；不复制未提交文件、不宣称导入已交付 |
| 原审计 | beta4 / `6a0e282` 的历史样本 | 原审计 JSON 未随文档入库，不能作为当前可运行闸门 |
| 目标 | v2.1 起按功能发布，v3.0 完成核心重构；当前目标图 46 包 | 新业务经所有权审查可调整图，包数不作为验收指标 |

当前产品仍为 daemon HTTP/SSE → runtime agent/ToolRuntime → session canonical + message archive，
客户端仍消费 Ringing v2 lease/driver；不是 major 3、唯一会话存储或新薄 loop 已完成。
业务线已新增 ChatGPT 订阅 / DeepSeek 账号、DeepSeek Chat/Responses、官方 Search/Fetch、账号客户端服务面、
缓存亲和、实时 usage、压缩结果隔离与原因展示、参数别名及写冲突保护等增量。
具体已核对能力、源码与验收去向以 [能力保留矩阵](v2-reset/preservation.md) 为准，不能只迁移 beta4。
当前已有多 profile 与会话名字选择，但 Config/API 仍以全局扁平活跃项为中心；credential 复制/热同步、
默认与会话隔离、密钥绑定和 child 继承尚有缺口，见 [多配置全链路](v2-reset/configuration.md)，不宣称现有多配置闭环完整。

现有 PTY writer/过程进度不等于新终端接管，分散诊断不等于独立 feed。
ACP、完整市场安装、资源 scope 热插拔、远端 ClientToolHost 与 major 3 SDK 仍为目标。
现有 Rust/Kotlin/ArkTS v2 消费者必须迁移，不能把它们写成尚无客户端，也不能以此宣称目标 SDK 已完成。
桌面 GenUI v0.2 是受限 UI 渲染协议，与 Ringing major 3 独立；重构不得丢失旧内容渲染、安全约束及用户交互路径。

## 2. 目标所有者

目标为模块化服务器：api → application → 薄 agent / config / access，
agent 通过契约使用 store/providers/executor/features，daemon 装配。
[消息与历史操作](v2-reset/messages.md) 定义细粒度 items、撤回与 fork；当前 msg_id/按 turn 截断不能当作目标语义。
Ringing major 3 定义 committed/live/diagnostics/terminal 四域；客户端工具使用独立 ClientTool WS。
UI 切换不迁移作业，daemon 仍是会话事实唯一提交端。

| 持久/运行数据 | 目标角色 | 所有者 |
|---|---|---|
| 会话 facts + commit marker + blobs | 唯一会话权威 | session-store |
| catalog intent/terminal | 目录聚合权威 | session-store/catalog |
| context/timeline/meta/team 索引 | 可重建投影 | session-store/read、subscriptions |
| messages.jsonl/WAL/SaveFull | 该存储域迁移后删除，离线 import 可读旧格式 | importer |
| config/profile/current、secret 版本与账号绑定 | 配置/凭据绑定权威 | config |
| provider account token/refresh、远端登录/校验/退出 | 模型服务账号权威，独立于设备授权 | providers/account，通过 config 凭据端口持久化 |
| device/grant/revocation | daemon 访问控制权威，不管理模型网站登录 | access |
| workspace journal/undo | 文件变更权威，独立于对话 | workspace |
| terminal 进程/PTY/fence/ring buffer | 易失执行态，结论入会话 facts | terminal |
| diagnostics 时序/窗口索引 | 附加诊断/可重建统计 | diagnostics |
| extension inventory/bindings/operations | 安装权威，独立于 session facts | extensions registry |
| client tool resources/execution receipts | 本设备权威；会话结果由 daemon 提交 | ClientToolHost |
| UI preferences / GenUI surface local state | 每端本地，不授予执行权限 | client app |
| 外部 transcript 原始快照、来源图与规范化包 | 导入功能的来源证据；正式会话仍由 session-store 提交 | import parser + application/import + session-store，待并行切片锁定 |
| lease/driver/global session/TLS maps | 对应消费者迁移后删除，不提前拔除 | 无终态所有者 |

账号及官方网页工具边界详见 [providers](v2-reset/providers.md)，目标图保留 siwc/dsaccount 的模块及迁移映射。
配置细化为命名 ProfileVersion/ModelBindingVersion、新会话默认、SessionSelection 与 turn/request 快照；
完整所有权、写入 CAS、用途继承和配置来源见 configuration；此处的 config 权威不等于单个全局 active 配置。
qaqh-import 尚未纳入已确认目标图；其纯契约按归属进入 kernel/context/session-api，不重新建立万能 types 或独立会话 writer。
原全局账号 manager 是待迁移实现，不是必须保留的架构。

## 3. 验证范围

[工程规则](v2-reset/engineering.md) 的补偿与测试样本来自 beta4，仅在原前提消除后退出；近期回归必须重新核对。
本仓 [audit.py](../../tools/v2-architecture/audit.py) 的 validate 验证目标图，check 对照实际 Cargo workspace 的全量终态，links 检查本目录本地文件链接。
check 的预期缺口是尚未完成的迁移，不代表运行期 bug 数；检查细节见工具 README。
中间版本按切片审查依赖/真实消费者并记录缺口，v3.0 全量 check 必须通过。
图/schema/离线向量不替代账号远端、commit/故障恢复、SSE/WS/PTY、真实 SDK、桌面和平台接缝。
本轮校准文档并提供只读结构检查；未切换 daemon、改动 Rust 产品代码或运行 Rust/多端回归。
安装 schema 与跨语言协议消费向量仍待对应切片实现，不能以结构检查代替。
