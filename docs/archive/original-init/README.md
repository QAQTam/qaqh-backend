# DeepX

AI Agent 桌面应用的 monorepo —— 包含 Rust 后端、Electron 前端、Windows 安装器。

## 项目结构

```
DeepX/
├── crates/                  # Rust 后端 (15 crates)
│   ├── deepx-daemon         后端守护进程 (入口)
│   ├── deepx-runtime        Agent 运行时
│   ├── deepx-message        消息上下文与持久化
│   ├── deepx-session        Session 管理
│   ├── deepx-config         配置与数据库镜像
│   ├── deepx-tools          文件、Shell、Git、Plan 等工具
│   ├── deepx-skills         Skills 发现与运行时
│   ├── deepx-subagent       子 Agent 能力
│   ├── deepx-gate           模型访问与流式输入
│   ├── deepx-types          公共领域类型
│   ├── deepx-proto          IPC 协议定义
│   ├── deepx-msglp          消息循环驱动
│   ├── deepx-client         WebSocket 客户端
│   ├── deepx-companion      同步引擎
│   └── deepx-gate-testui    Gate 测试界面
├── apps/
│   ├── desktop/             Electron + Vite + SolidJS 前端
│   └── installer/           Windows 原生安装器 (egui)
├── docs/                    架构文档
├── skills/                  项目 Skills
├── scripts/                 构建辅助脚本
├── justfile                 统一构建入口
├── version.txt              单一版本号源
└── Cargo.toml               Rust workspace
```

## 快速开始

### 环境要求

- Rust stable (edition 2024)
- Node.js >= 22, pnpm >= 11
- [just](https://github.com/casey/just) (推荐)
- Windows (安装器/桌面打包目前仅支持 Windows)

### 克隆

```powershell
git clone https://github.com/QAQTam/DeepX.git
cd DeepX
```

### 构建

```powershell
# 编译后端
just build-daemon

# 编译安装器
just build-installer

# 构建前端（typecheck + vite）
just build-desktop

# 完整流水线（daemon → desktop electron-builder → installer SFX）
just package
```

### 开发

```powershell
# 启动 daemon
just dev

# 启动前端开发模式（需先 build-daemon）
just dev-desktop

# 初始化前端依赖
just setup
```

### 常用命令

| 命令 | 说明 |
|---|---|
| `just build-daemon` | 编译 daemon (release) |
| `just build-installer` | 编译安装器 (release) |
| `just build-desktop` | 前端 typecheck + vite 构建 |
| `just package` | 完整打包流水线 |
| `just check` | 全部静态检查 (Rust + TypeScript) |
| `just test` | 全部测试 |
| `just fmt` | Rust 格式化检查 |
| `just clippy` | Rust Clippy |
| `just status` | 产物状态 |
| `just clean` | 清理构建产物 |
| `just sync-version` | 从 version.txt 同步版本号 |

## 版本号

`version.txt` 是单一版本号源。运行 `just sync-version` 将版本号下发到 Cargo.toml、package.json、deepx-backend.lock.json。

当前版本：**0.9.0**

## CI / Release

- **ci.yml** — PR 检查：push/pull_request 触发，路径过滤
  - Rust: cargo check + test + fmt + clippy
  - Desktop: pnpm install + typecheck + test + build
- **release.yml** — 发布：tag push `v*` 触发
  - Daemon 三平台编译
  - Desktop Windows 安装包
  - GitHub Release 自动发布

## 前身仓库

本仓库由以下三个独立仓库归一化而成：

- [D:\DeepX] — 原始后端 workspace（现在 crates/）
- [D:\deepx-desktop] — 原始前端（现在 apps/desktop/）
- [D:\DeepXInstaller] — 原始安装器（现在 apps/installer/）

旧仓库目录中已放置 `DEPRECATED.md`，所有开发请在本 monorepo 进行。
