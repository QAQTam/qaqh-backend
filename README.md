# DeepX

DeepX 是一款本地优先的 AI Agent 桌面应用，由 Rust 后端、Electron + SolidJS 前端以及原生 Windows 安装与更新组件组成。

当前版本：**1.0.0-beta.5**

> **DeepX 1.0 是 DeepX-Ring 版本。**它以统一的 Ringing V1 协议与架构作为桌面端、daemon 与 worker 的正式通信基础。

> 项目仍处于 beta 阶段。API、协议和持久化格式将在若干 beta 版本验证后，随 DeepX-Ring `1.0.0` 正式版进入稳定兼容周期。

## 核心能力

- 流式模型对话、token 用量与缓存命中统计
- Ringing 原生控制协议：会话快照、命令确认、批量事件与 SSE 增量同步
- Ringing V1 Timeline：按会话严格排序、可持久化回放的思考、聊天与工具时间线
- Markdown 增量渲染与分段封口，避免流式半截标记造成的吞字与显示错位
- 文件、Shell、Git、Plan、Todo 和网页访问等工具
- 会话持久化、工作区管理、Skills 和子 Agent
- Electron 桌面端与独立 Rust daemon
- 前端、后端和完整组件的独立打包与更新
- 统一 updater，负责修改、升级、回滚和安全卸载
- 本地优先配置、日志与审计数据

DeepX 可连接 DeepSeek、OpenAI 及其他兼容模型服务。API Key 由用户自行配置，仅用于向所选模型服务鉴权。

## 项目结构

```text
DeepX/
├── .deepx/skills/          # DeepX 运行时可发现的项目级 Skills
├── .codex/skills/          # Codex 开发与审计 Skills
├── apps/
│   ├── desktop/            # Electron + Vite + SolidJS 前端
│   ├── installer/          # egui 原生 Windows 安装器
│   └── updater/            # 统一更新、维护和卸载程序
├── crates/
│   ├── deepx-daemon        # 后端守护进程
│   ├── deepx-runtime       # 应用服务与运行时
│   ├── deepx-msglp         # Agent 消息循环
│   ├── deepx-gate          # 模型访问与流式响应
│   ├── deepx-session       # 会话管理与持久化
│   ├── deepx-workspace     # 工具、权限与执行审计
│   ├── deepx-skills        # Skills 发现与生命周期
│   ├── deepx-subagent      # 子 Agent
│   ├── deepx-update        # 更新协议与事务引擎
│   └── ...                 # 公共类型、协议、客户端等
├── docs/legal/             # 用户协议、隐私政策与同意记录 Schema
├── scripts/                # 构建与版本同步脚本
├── justfile                # 统一构建入口
├── version.txt             # DeepX 单一版本号源
└── Cargo.toml              # Rust workspace
```

## 开发环境

- Windows 10/11
- Rust stable
- Node.js 22 或更高版本
- pnpm 11 或更高版本
- [just](https://github.com/casey/just)

桌面安装包目前面向 Windows。Rust 核心组件保留跨平台构建能力，但 Linux/macOS 的桌面打包流程尚未完成。

## 快速开始

```powershell
git clone https://github.com/QAQTam/DeepX.git
cd DeepX

# 安装前端依赖
just setup

# 编译后端 daemon
just build-daemon

# 启动桌面开发模式
just dev-desktop
```

单独启动 daemon：

```powershell
just dev
```

## 构建与测试

| 命令 | 说明 |
|---|---|
| `just build-daemon` | 构建 Rust daemon |
| `just build-desktop` | 前端类型检查与 Vite 构建 |
| `just build-installer` | 构建原生安装器 |
| `just build-updater` | 构建统一 updater |
| `just package` | 生成完整 Windows 安装包 |
| `just package-update-frontend` | 生成仅前端的本地更新源 |
| `just package-update-backend` | 生成仅后端的本地更新源 |
| `just package-update` | 生成前端、后端和完整更新源 |
| `just check` | Rust workspace 与前端静态检查 |
| `just test` | Rust 与前端测试 |
| `just fmt` | Rust 格式检查 |
| `just clippy` | Rust Clippy |
| `just status` | 查看构建产物 |
| `just clean` | 清理生成文件 |

## Beta.5 协议说明

Beta.5 开始，Electron 桌面端使用 Ringing 与 Ringing V1 Timeline 读取运行状态和对话时间线：

- 每个会话的事件使用严格递增 cursor；重连后可从 cursor 恢复，不依赖前端临时缓冲。
- 思考链、聊天正文和工具生命周期保留同一条时间线中的精确先后关系。
- Markdown 内容随 text delta 增量进入渲染器；区块封口只标记其完成状态，不再阻断流式展示。
- Electron 已不再使用旧 `Agent2Ui` / `Ui2Agent` 协议；桌面端必须与支持 Ringing/Ringing V1 Timeline 的 daemon 配套使用。

TUI 与 WinUI3 的迁移不包含在 Beta.5 的 Electron 协议切换范围内。Beta.5 仍为预发布版本，合并与 Release 前须完成已打包 Electron 的干净环境冒烟验证。

## 版本管理

`version.txt` 是 DeepX 发布版本的单一来源：

```powershell
just sync-version
```

该命令会同步 Rust workspace、Electron、后端锁文件和 Release manifest URL。项目内置的版本审计脚本还会检查 TUI、安装器及 Windows `DisplayVersion` 链路。

协议文档使用独立版本号，位于 `docs/legal/version.txt`。安装器会将用户同意状态、文档版本和文档哈希写入本地 `legal-consent.json`，供后续 Electron 协议更新提示复用。

## 安装与更新

完整安装包由 Electron 的 unpacked 运行目录和 DeepX 原生安装器组装，不使用 Electron NSIS 安装流程。

`deepx-updater.exe` 是唯一的维护入口，统一负责：

- 前端、后端或完整组件更新
- 更新暂存、应用和回滚
- Windows“修改”入口
- 安全卸载与可选用户数据清理

独立 `uninstall.exe` 已移除。Windows 卸载注册表直接调用 updater；完整安装或修复会在验证安装根后清理旧版本遗留的兼容卸载器。

## 数据与隐私

DeepX 默认将配置、会话、日志和工具审计保存在当前用户的本地数据目录。模型请求、搜索、网页访问和在线更新仅在用户触发相关功能时访问对应第三方服务。

发布前请阅读：

- [DeepX 用户协议](docs/legal/USER_AGREEMENT.zh-CN.md)
- [DeepX 隐私政策](docs/legal/PRIVACY_POLICY.zh-CN.md)

## License

DeepX 依据 [MIT License](LICENSE) 开源。第三方组件分别适用其各自许可证。
