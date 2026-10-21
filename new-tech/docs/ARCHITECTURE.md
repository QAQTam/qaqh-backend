# 实现状态与架构目标

校准：2026-10-15。现行设计入口：[v2-reset](v2-reset/README.md)；发布规则见 [rollout](v2-reset/rollout.md)。
**当前产品不再冻结；again 设计工作树不等于当前产品；以下目标不表示已实现。**

## 当前工作树配置切片

CFG-01 从 backend `80ddb3c` / RC2 / 33 members 开工，配置资源只读契约、显式路径 reader、
admin service 映射及 Rust typed client 见 [现行配置契约](v2-reset/configuration.md)。
本工作树尚未交付资源写入/CAS、凭据版本、会话 applied/pending 或多端 UI 迁移；下面 again 表格为历史核对对象。

## 1. 基线与状态

| 对象 | 核对结果 | 使用方式 |
|---|---|---|
| again 设计工作树 | `v2.0-again`，本轮起点 `13d1e59`，Cargo 仍 `2.0.0-alpha10` / 30 members | 保存终态设计，不作为业务迁移起点 |
| 当前 backend | `main`，`91b2fd9`，Cargo `2.0.0-RC2` / 32 members | 当前能力证据；本轮读取时 Cargo.lock 有本地变更，未改动它 |
| 当前 desktop | `8a36e658f6b05841ab598b837c42aab60eeb8d28` | 与后端联合迁移，具体能力与证据见保留矩阵 |
| 原审计 | beta4 / `6a0e282`、tools/v2-architecture/baseline.json | 历史样本，不冒充当前产品状态或最新测试规模 |
| 目标 | v2.1 起按功能发布，v3.0 完成核心重构；46-crate 目标 DAG | 功能切片逐步达到，不要求 v2.1 一次性切换 |

当前产品仍为 daemon HTTP/SSE → runtime agent/ToolRuntime → session canonical + message archive，
客户端仍消费 Ringing v2 lease/driver；不是 major 3、唯一会话存储或新薄 loop 已完成。
业务线已新增 ChatGPT 订阅 / DeepSeek 账号、模型与推理能力、工具参数容错、调用身份与失败展示、工作区审计等增量。
具体已核对能力、源码与验收去向以 [能力保留矩阵](v2-reset/preservation.md) 为准，不能只迁移 beta4。

现有 PTY writer/过程进度不等于新终端接管，分散诊断不等于独立 feed。
ACP、完整市场安装、资源 scope 热插拔、远端 ClientToolHost 与多语言 SDK 仍为目标，不因已有 provider 登录或桌面连接而宣称完成。
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
| lease/driver/global session/TLS maps | 对应消费者迁移后删除，不提前拔除 | 无终态所有者 |

账号边界详见 [providers](v2-reset/providers.md)，保留 46 包目标但补齐 siwc/dsaccount 的模块及迁移映射。
原全局账号 manager 是待迁移实现，不是必须保留的架构。

## 3. 验证范围

[工程规则](v2-reset/engineering.md) 的补偿与测试样本来自 beta4，仅在原前提消除后退出；近期回归必须重新核对。
`audit.py validate` 只验证目标图；`check` 对照全量终态，未迁移工作树预期存在缺口，不代表运行期 bug 数。
中间版本按切片审查依赖/真实消费者并记录缺口，v3.0 全量 check 必须通过。
图/schema/离线向量不替代账号远端、commit/故障恢复、SSE/WS/PTY、真实 SDK、桌面和平台接缝。
本轮只校准文档，没有切换 daemon、合并业务代码或运行 Rust/桌面回归；执行结果以实际验证报告为准。
