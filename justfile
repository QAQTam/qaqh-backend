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
build-installer:
    cargo build --release -p deepx-installer

# 构建前端（typecheck + vite，不含 daemon）
build-desktop:
    Set-Location apps/desktop; pnpm build

# ── 打包 ────────────────────────────────────────────

# 打包桌面 Electron App（daemon 编译 + 侧车注入 + electron-builder）
[windows]
package-desktop: build-daemon
    Set-Location apps/desktop; node scripts/prepare-daemon.mjs --backend-root ../..
    Set-Location apps/desktop; pnpm build
    Set-Location apps/desktop; pnpm exec electron-builder --dir --win --x64 --publish never

# 生成安装器 SFX（安装器编译 + 收集产物 + 拼接）
[windows]
package-installer: build-installer
    ./apps/installer/scripts/collect-payload.ps1
    ./apps/installer/scripts/finalize.ps1

# 完整流水线
[windows]
package: package-desktop package-installer

# SFX 快速拼接（payload 已就位，跳过编译）
[windows]
sfx-quick:
    ./apps/installer/scripts/collect-payload.ps1
    ./apps/installer/scripts/finalize.ps1

# ── 开发 ────────────────────────────────────────────

# 启动 daemon（dev profile）
dev:
    cargo run -p deepx-daemon -- run

# 启动桌面开发模式（需先 build-daemon 或设 DEEPX_BACKEND_ROOT）
dev-desktop:
    Set-Location apps/desktop; pnpm dev

# ── 检查 & 测试 ─────────────────────────────────────

# Rust workspace 检查
check-rust:
    cargo check --workspace

# 前端类型检查
check-desktop:
    Set-Location apps/desktop; pnpm typecheck

# 全部静态检查
check: check-rust check-desktop

# 全部测试
test:
    cargo test --workspace
    Set-Location apps/desktop; pnpm test

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
    @"if (Test-Path 'target/release/deepx-daemon.exe') { '  ✓ deepx-daemon.exe' } else { '  ✗ deepx-daemon.exe' }"
    @"if (Test-Path 'target/release/deepx-companion.exe') { '  ✓ deepx-companion.exe' } else { '  ✗ deepx-companion.exe' }"
    @"if (Test-Path 'target/release/DeepXInstaller.exe') { '  ✓ DeepXInstaller.exe' } else { '  ✗ DeepXInstaller.exe' }"
    @"if (Test-Path 'target/release/deepx-uninstaller.exe') { '  ✓ deepx-uninstaller.exe' } else { '  ✗ deepx-uninstaller.exe' }"
    @Write-Output "=== Desktop ==="
    @"if (Test-Path 'apps/desktop/out/main/main.js') { '  ✓ main.js' } else { '  ✗ main.js' }"
    @"if (Test-Path 'apps/desktop/out/renderer/index.html') { '  ✓ renderer' } else { '  ✗ renderer' }"
    @Write-Output "=== Payload ==="
    @"if (Test-Path 'apps/installer/payload/config/default.toml') { '  ✓ config' } else { '  ✗ config' }"

# 清理
[windows]
clean:
    cargo clean
    @"Remove-Item -Recurse -Force 'apps/desktop/out' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/desktop/release' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/desktop/build/sidecar' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/installer/dist' -ErrorAction SilentlyContinue"
    @"Remove-Item -Recurse -Force 'apps/installer/payload/desktop' -ErrorAction SilentlyContinue"
    @Write-Output "Clean done."

# 初始化开发环境
setup:
    Set-Location apps/desktop; pnpm install
    @Write-Output "Setup done. Run 'just build-daemon' to compile the backend."

# 从 version.txt 同步版本号到所有配置文件
[windows]
sync-version:
    @pwsh -File scripts/sync-version.ps1

# ── Linux/macOS (stub) ─────────────────────────────
[unix]
package-desktop:
    @echo "TODO: Linux desktop packaging not implemented"
    @exit 1

[unix]
package-installer:
    @echo "TODO: Linux installer not implemented"
    @exit 1

[unix]
package:
    @echo "TODO: Linux packaging not implemented"
    @exit 1

[unix]
clean:
    cargo clean
    rm -rf apps/desktop/out apps/desktop/release apps/desktop/build/sidecar
    rm -rf apps/installer/dist apps/installer/payload/desktop
    @echo Clean done.
