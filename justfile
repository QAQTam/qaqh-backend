# DeepX Monorepo — 后端统一构建系统
# 用法: just [recipe]
#
# 项目结构:
#   crates/          Rust 后端 (18 crates)
#   dsh-minimal-mode/ 极简模式工具（bash_v2 + str_replace_editor）
#
# 说明：Windows 桌面层（WinUI3 壳 / installer / updater）已拆分为独立仓库
# F:\deepx-winui-app；本仓库只保留跨平台后端核心与公共 SDK。
# 前端打包用 ../deepx-winui-app/justfile。

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# ── 默认 ────────────────────────────────────────────
default:
    @just --list

# ── 构建 ────────────────────────────────────────────

# 编译 daemon（后端核心，release）
build-daemon:
    cargo build --release -p deepx-daemon -p deepx-workspace

# ── 开发 ────────────────────────────────────────────

# 启动 daemon（dev profile）
dev:
    cargo run -p deepx-daemon -- run

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
    @if (Test-Path 'target/release/deepx-daemon.exe') { '  ✓ deepx-daemon.exe' } else { '  ✗ deepx-daemon.exe' }
    @if (Test-Path 'target/release/deepx-workspace.exe') { '  ✓ deepx-workspace.exe' } else { '  ✗ deepx-workspace.exe' }

# 清理
clean:
    cargo clean

# 从 version.txt 同步版本号到所有后端配置文件
[windows]
sync-version:
    @pwsh -File scripts/sync-version.ps1
