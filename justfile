# DeepX Monorepo — 统一构建系统
# 用法: just [recipe]
#
# 项目结构:
#   crates/          Rust 后端 (17 crates)
#   apps/winui/      WinUI3 原生桌面壳（windows-reactor，全原生 XAML 视图族）
#   apps/installer/  Windows 安装器
#   apps/updater/    统一更新/维护组件
#
# 说明：WebView/renderer（SolidJS）已整体移除，前端为纯 WinUI3 原生实现，
# 构建链不依赖 node/pnpm。

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# ── 默认 ────────────────────────────────────────────
default:
    @just --list

# ── 构建 ────────────────────────────────────────────

# 编译 daemon（后端核心，release）
build-daemon:
    cargo build --release -p deepx-daemon -p deepx-workspace

# 编译安装器（release）
[windows]
build-installer:
    cargo build --release -p deepx-installer

# 编译组件更新器（release）
build-updater:
    cargo build --release -p deepx-updater

# ── 打包（winui 壳）────────────────────────────────

# 编译 winui 壳（release）
[windows]
build-winui:
    cargo build --release -p deepx-winui

# 打包 winui 运行目录（release/winui-app，完整安装包使用）
[windows]
package-winui-desktop: build-daemon build-winui
    pwsh -File apps/winui/scripts/prepare-daemon.ps1
    ./apps/winui/scripts/assemble-winui.ps1

# 生成完整安装包 EXE（winui 壳；效果等同 just package）
[windows]
winui-package: package-winui-desktop build-installer build-updater
    ./apps/installer/scripts/collect-payload-winui.ps1 -Kind full
    ./apps/installer/scripts/finalize.ps1 -Kind full

# SFX 快速拼接（staging 已就位，跳过构建和收集）
[windows]
sfx-quick kind="full":
    ./apps/installer/scripts/finalize.ps1 -Kind {{kind}}

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
    @if (Test-Path 'target/release/deepx-winui.exe') { '  ✓ deepx-winui.exe' } else { '  ✗ deepx-winui.exe' }
    @if (Test-Path 'target/release/DeepXInstaller.exe') { '  ✓ DeepXInstaller.exe' } else { '  ✗ DeepXInstaller.exe' }
    @if (Test-Path 'target/release/deepx-updater.exe') { '  ✓ deepx-updater.exe' } else { '  ✗ deepx-updater.exe' }
    @Write-Output "=== WinUI run dir ==="
    @if (Test-Path 'apps/winui/release/winui-app/DeepX.exe') { '  ✓ winui-app/DeepX.exe' } else { '  ✗ winui-app (run just package-winui-desktop)' }
    @Write-Output "=== Packages ==="
    @if (Test-Path 'apps/installer/staging/builds') { Get-ChildItem 'apps/installer/staging/builds' -Recurse -Filter 'bundle.json' | ForEach-Object { "  ✓ $($_.Directory.Parent.Name)/$($_.Directory.Name)" } } else { '  ✗ no packages yet' }

# 清理
[windows]
clean:
    cargo clean
    @Remove-Item -Recurse -Force 'apps/winui/out' -ErrorAction SilentlyContinue
    @Remove-Item -Recurse -Force 'apps/winui/release/winui-app' -ErrorAction SilentlyContinue
    @Remove-Item -Recurse -Force 'packages' -ErrorAction SilentlyContinue
    @Remove-Item -Recurse -Force 'apps/installer/dist' -ErrorAction SilentlyContinue
    @Remove-Item -Recurse -Force 'apps/installer/staging' -ErrorAction SilentlyContinue
    @Remove-Item -Recurse -Force 'apps/installer/payload/desktop' -ErrorAction SilentlyContinue
    @Write-Output "Clean done."

# 从 version.txt 同步版本号到所有配置文件
[windows]
sync-version:
    @pwsh -File scripts/sync-version.ps1
