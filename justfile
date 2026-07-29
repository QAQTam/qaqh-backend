# DeepX Monorepo — 统一构建系统
# 用法: just [recipe]
#
# 项目结构:
#   crates/          Rust 后端 (15 crates)
#   apps/desktop/    Electron 前端
#   apps/installer/  Windows 安装器

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

# ── 默认 ────────────────────────────────────────────
default:
    @just --list

# ── 构建 ──────────────────────────────��─────────────

# 编译 daemon（后端核心，release）
build-daemon:
    cargo build --release -p deepx-daemon

# 编译 companion（同步引擎，release）
build-companion:
    cargo build --release -p deepx-companion

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
    Set-Location apps/desktop; pnpm build

# ── 打包 ────────────────────────────────────────────

# 打包桌面 Electron 运行目录（仅完整安装包使用）
[windows]
package-desktop: build-daemon
    Set-Location apps/desktop; node scripts/prepare-daemon.mjs --backend-root ../..
    Set-Location apps/desktop; pnpm build
    Set-Location apps/desktop; pnpm exec electron-builder --dir --win --x64 --publish never

# 只构建前端 ASAR，不构建 Electron Runtime 或 Rust 后端
[windows]
pack-frontend: build-desktop
    Set-Location apps/desktop; node scripts/pack-frontend.mjs

# 生成完整安装包 EXE（首次安装、修复或完整升级）
[windows]
package: package-desktop build-installer build-updater
    ./apps/installer/scripts/collect-payload.ps1 -Kind full
    ./apps/installer/scripts/finalize.ps1 -Kind full

# 生成仅前端的本地更新源（catalog.json + Bundle ZIP）
[windows]
package-update-frontend: pack-frontend build-installer
    ./apps/installer/scripts/collect-payload.ps1 -Kind frontend
    ./apps/installer/scripts/make-update-source.ps1 -Kinds frontend

# 生成仅后端的本地更新源（catalog.json + Bundle ZIP）
[windows]
package-update-backend: build-daemon build-installer
    Set-Location apps/desktop; node scripts/prepare-daemon.mjs --backend-root ../..
    ./apps/installer/scripts/collect-payload.ps1 -Kind backend
    ./apps/installer/scripts/make-update-source.ps1 -Kinds backend

# 生成智能本地更新源（Full + Frontend + Backend）
[windows]
package-update: package-desktop build-installer build-updater
    ./apps/installer/scripts/collect-payload.ps1 -Kind full
    ./apps/installer/scripts/collect-payload.ps1 -Kind frontend -FrontendAsarPath apps/desktop/release/win-unpacked/resources/app.asar
    ./apps/installer/scripts/collect-payload.ps1 -Kind backend
    ./apps/installer/scripts/make-update-source.ps1 -Kinds full,frontend,backend

# ── 旧打包命令（可调用，但不在 just --list 中显示）────

[private]
[windows]
package-frontend: pack-frontend build-installer
    ./apps/installer/scripts/collect-payload.ps1 -Kind frontend
    ./apps/installer/scripts/finalize.ps1 -Kind frontend

[private]
[windows]
package-backend: build-daemon build-installer
    Set-Location apps/desktop; node scripts/prepare-daemon.mjs --backend-root ../..
    ./apps/installer/scripts/collect-payload.ps1 -Kind backend
    ./apps/installer/scripts/finalize.ps1 -Kind backend

[private]
[windows]
package-full: package

[private]
[windows]
package-installer: package

[private]
[windows]
package-update-full: package
    ./apps/installer/scripts/make-update-source.ps1 -Kinds full

[private]
[windows]
package-update-all: package-update

# SFX 快速拼接（staging 已就位，跳过构建和收集）
[windows]
sfx-quick kind="full":
    ./apps/installer/scripts/finalize.ps1 -Kind {{kind}}

# ── 开发 ────────────────────────────────────────────

# 启动 daemon（dev profile）
dev:
    cargo run -p deepx-daemon -- run

# 启动桌面开发模式（需先 build-daemon 或设 DEEPX_BACKEND_ROOT）
[windows]
dev-desktop:
    Set-Location apps/desktop; pnpm dev

# ── 检查 & 测试 ─────────────────────────────────────

# Rust workspace 检查
check-rust:
    cargo check --workspace

# 前端类型检查
[windows]
check-desktop:
    Set-Location apps/desktop; pnpm typecheck

# 全部静态检查
[windows]
check: check-rust check-desktop
[unix]
check: check-rust

# 全部测试
[windows]
test:
    cargo test --workspace
    Set-Location apps/desktop; pnpm test
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
    @if (Test-Path 'target/release/deepx-companion.exe') { '  ✓ deepx-companion.exe' } else { '  ✗ deepx-companion.exe' }
    @if (Test-Path 'target/release/DeepXInstaller.exe') { '  ✓ DeepXInstaller.exe' } else { '  ✗ DeepXInstaller.exe' }
    @if (Test-Path 'target/release/deepx-updater.exe') { '  ✓ deepx-updater.exe' } else { '  ✗ deepx-updater.exe' }
    @Write-Output "=== Desktop ==="
    @if (Test-Path 'apps/desktop/out/main/main.js') { '  ✓ main.js' } else { '  ✗ main.js' }
    @if (Test-Path 'apps/desktop/out/renderer/index.html') { '  ✓ renderer' } else { '  ✗ renderer' }
    @Write-Output "=== Packages ==="
    @if (Test-Path 'packages') { Get-ChildItem 'packages' -Force | ForEach-Object { "  ✓ $($_.Name)" } } else { '  ✗ no packages yet' }

# 清理
[windows]
clean:
    cargo clean
    @"Remove-Item -Recurse -Force 'apps/desktop/out' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/desktop/release' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/desktop/build/sidecar' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'packages' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/installer/dist' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/installer/staging' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/installer/payload/desktop' -ErrorAction SilentlyContinue"
    @Write-Output "Clean done."

# 初始化开发环境
[windows]
setup:
    Set-Location apps/desktop; pnpm install
    @Write-Output "Setup done. Run 'just build-daemon' to compile the backend."

# 从 version.txt 同步版本号到所有配置文件
[windows]
sync-version:
    @pwsh -File scripts/sync-version.ps1

# ── Linux ───────────────────────────────────────────

[unix]
package-desktop: build-daemon
    cd apps/desktop && node scripts/prepare-daemon.mjs --backend-root ../..
    cd apps/desktop && pnpm build
    cd apps/desktop && pnpm exec electron-builder --dir --linux --x64 --publish never

[unix]
build-installer:
    @echo "deepx-installer crate is Windows-only (uses COM, Registry, tasklist)"
    @echo "See crates/deepx-msglp for how to cfg-gate the windows dependency."
    @exit 1

[unix]
package: package-desktop
    @echo "  ✓ Electron app packaged to release/"
    @echo "  ⚠ Full installer (NSIS + SFX) is Windows-only; skipped"

[unix]
build-desktop:
    cd apps/desktop && pnpm build

[unix]
dev-desktop:
    cd apps/desktop && pnpm dev

[unix]
check-desktop:
    cd apps/desktop && pnpm typecheck

[unix]
pack-frontend: build-desktop
    cd apps/desktop && node scripts/pack-frontend.mjs

[unix]
clean:
    cargo clean
    rm -rf apps/desktop/out apps/desktop/release apps/desktop/build/sidecar
    rm -rf packages apps/installer/dist apps/installer/staging apps/installer/payload/desktop
    @echo Clean done.

[unix]
setup:
    cd apps/desktop && pnpm install
    @echo "Setup done. Run 'just build-daemon' to compile the backend."

[unix]
status:
    @echo "=== Rust binaries ==="
    @test -f target/release/deepx-daemon && echo "  ✓ deepx-daemon" || echo "  ✗ deepx-daemon"
    @test -f target/release/deepx-companion && echo "  ✓ deepx-companion" || echo "  ✗ deepx-companion"
    @echo "=== Desktop ==="
    @test -f apps/desktop/out/main/main.js && echo "  ✓ main.js" || echo "  ✗ main.js"
    @test -f apps/desktop/out/renderer/index.html && echo "  ✓ renderer" || echo "  ✗ renderer"
    @echo "=== Packages ==="
    @ls -la packages 2>/dev/null || echo "  ✗ no packages yet"
