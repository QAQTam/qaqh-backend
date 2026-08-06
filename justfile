# DeepX Monorepo — 统一构建系统
# 用法: just [recipe]
#
# 项目结构:
#   crates/          Rust 后端 (16 crates)
#   apps/winui/renderer/  Web renderer 源码（由 winui 壳承载）
#   apps/winui/out/renderer/  构建产物（唯一产物目录）
#   apps/installer/  Windows 安装器

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# ── 默认 ────────────────────────────────────────────
default:
    @just --list

# ── 构建 ──────────────────────────────��─────────────

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

# 构建前端（typecheck + vite，不含 daemon）
[windows]
build-desktop:
    Set-Location apps/winui/renderer; pnpm build

# ── 打包（winui 壳）────────────────────────────────

# 编译 winui 壳（release）+ 注入桥脚本到 renderer 产物
[windows]
build-winui: build-desktop
    cargo build --release -p deepx-winui
    node apps/winui/scripts/patch-renderer.mjs

# 打包 winui 运行目录（release/winui-app，完整安装包使用）
[windows]
package-winui-desktop: build-daemon build-winui
    Set-Location apps/winui/renderer; node scripts/prepare-daemon.mjs --backend-root ../../..
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

# 启动 renderer dev server（winui 壳用 DEEPX_DEBUG_URL 指向它）
[windows]
dev-desktop:
    Set-Location apps/winui/renderer; pnpm dev

# ── 检查 & 测试 ─────────────────────────────────────

# Rust workspace 检查
check-rust:
    cargo check --workspace

# 前端类型检查
[windows]
check-desktop:
    Set-Location apps/winui/renderer; pnpm typecheck

# 全部静态检查
[windows]
check: check-rust check-desktop
[unix]
check: check-rust

# 全部测试
[windows]
test:
    cargo test --workspace
    Set-Location apps/winui/renderer; pnpm test
[unix]
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

# ── 工具 ──────────────────────────────��─────────────

# 产物状态
[windows]
status:
    @Write-Output "=== Rust binaries ==="
    @if (Test-Path 'target/release/deepx-daemon.exe') { '  ✓ deepx-daemon.exe' } else { '  ✗ deepx-daemon.exe' }
    @if (Test-Path 'target/release/DeepXInstaller.exe') { '  ✓ DeepXInstaller.exe' } else { '  ✗ DeepXInstaller.exe' }
    @if (Test-Path 'target/release/deepx-updater.exe') { '  ✓ deepx-updater.exe' } else { '  ✗ deepx-updater.exe' }
    @Write-Output "=== Renderer ==="
    @if (Test-Path 'apps/winui/out/renderer/index.html') { '  ✓ renderer' } else { '  ✗ renderer' }
    @Write-Output "=== Packages ==="
    @if (Test-Path 'packages') { Get-ChildItem 'packages' -Force | ForEach-Object { "  ✓ $($_.Name)" } } else { '  ✗ no packages yet' }

# 清理
[windows]
clean:
    cargo clean
    @"Remove-Item -Recurse -Force 'apps/winui/out' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/winui/renderer/build/sidecar' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'packages' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/installer/dist' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/installer/staging' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/installer/payload/desktop' -ErrorAction SilentlyContinue"
    @Write-Output "Clean done."

# 初始化开发环境
[windows]
setup:
    Set-Location apps/winui/renderer; pnpm install
    @Write-Output "Setup done. Run 'just build-daemon' to compile the backend."

# 从 version.txt 同步版本号到所有配置文件
[windows]
sync-version:
    @pwsh -File scripts/sync-version.ps1

# ── Linux ───────────────────────────────────────────

[unix]
build-desktop:
    cd apps/winui/renderer && pnpm build

[unix]
dev-desktop:
    cd apps/winui/renderer && pnpm dev

[unix]
check-desktop:
    cd apps/winui/renderer && pnpm typecheck

[unix]
clean:
    cargo clean
    rm -rf apps/winui/out apps/winui/renderer/build/sidecar
    rm -rf packages apps/installer/dist apps/installer/staging apps/installer/payload/desktop
    @echo Clean done.

[unix]
setup:
    cd apps/winui/renderer && pnpm install
    @echo "Setup done. Run 'just build-daemon' to compile the backend."

[unix]
status:
    @echo "=== Rust binaries ==="
    @test -f target/release/deepx-daemon && echo "  ✓ deepx-daemon" || echo "  ✗ deepx-daemon"
    @echo "=== Renderer ==="
    @test -f apps/winui/out/renderer/index.html && echo "  ✓ renderer" || echo "  ✗ renderer"
    @echo "=== Packages ==="
    @ls -la packages 2>/dev/null || echo "  ✗ no packages yet"
