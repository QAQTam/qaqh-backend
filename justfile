# QAQ-Harness Monorepo — 后端统一构建系统
# 用法: just [recipe]
#
# 项目结构:
#   crates/          Rust 后端 (17 crates)
#
# 说明：Windows 桌面层（WinUI3 壳 / installer / updater）已拆分为独立仓库
# F:\qaqh-winui-app；本仓库只保留跨平台后端核心与公共 SDK。
# 前端打包用 ../qaqh-winui-app/justfile。

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# ── 默认 ────────────────────────────────────────────
default:
    @just --list

# ── 构建 ────────────────────────────────────────────

# 编译 daemon（后端核心，release）
build-daemon:
    cargo build --release -p qaqh-daemon

# ── 开发 ────────────────────────────────────────────

# 启动 daemon（dev profile）
dev:
    cargo run -p qaqh-daemon -- run

# ── 桌面客户端（已拆仓）────────────────────────────
#
# Tauri 2 壳 + SolidJS 渲染层（原 `webui/`）已于 2026-10-07 抽成独立仓
# **qaqh-desktop-app**（同级目录 `../qaqh-desktop-app`，含完整 git 历史）。
# 本仓不再包含、也不构建前端。以下能力随之移交到那边的 justfile：
#   renderer-build / ts-export / ts-check / place-sidecar / desktop-dev / desktop-build
#
# 唯一仍需本仓知道的事：**类型契约是单一真相**。`src/api/qaqh/*.ts`（那边仓里）
# 由本仓 crates/qaqh-{types,domain,ringing,session,config-api} 上的 `derive(TS)`
# 导出，靠 `TS_RS_EXPORT_DIR` 收口。因此改了 wire 类型后，去那边跑
# `just ts-export` 并提交那边的生成物——本仓不持有生成物，也没有漂移闸。

# ── 检查 & 测试 ─────────────────────────────────────

# Rust workspace 检查
check-rust:
    cargo check --workspace

# 全部静态检查
check: check-rust

# 全部测试
test:
    cargo test --workspace

# Rust 测试
test-rust:
    cargo test --workspace

# Rust 格式化检查
fmt:
    cargo fmt --all --check

# Rust Clippy
clippy:
    cargo clippy --workspace --all-targets

# ── 工具 ────────────────────────────────────────────

# 产物状态
[windows]
status:
    @Write-Output "=== Rust binaries ==="
    @if (Test-Path 'target/release/qaqh-daemon.exe') { '  ✓ qaqh-daemon.exe' } else { '  ✗ qaqh-daemon.exe' }

# 清理
clean:
    cargo clean

# 从 version.txt 同步版本号到所有后端配置文件
[windows]
sync-version:
    @pwsh -File scripts/sync-version.ps1
