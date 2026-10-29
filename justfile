# QAQ-Harness Monorepo — 后端统一构建系统
# 用法: just [recipe]
#
# 项目结构:
#   crates/          Rust 后端 (17 crates)
#
# 说明：跨平台桌面客户端为同级仓库 qaqh-electron（Linux 及多端第一公民）；
# 本仓库只保留跨平台后端核心与公共 SDK。
# 前端构建与打包由 ../qaqh-electron 负责。

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
# 跨平台桌面客户端已统一收敛至第一公民仓库 **qaqh-electron**（同级目录 `../qaqh-electron`）。
# 本仓专注于跨平台后端守护进程（daemon），不再包含前端构建。
# 类型契约由本仓 qaqh-contract-export 统一生成并向 qaqh-sdk / qaqh-electron 导出。

# 导出 SDK 权威契约（目录必须由本 exporter 管理或尚不存在）
sdk-export out:
    cargo run -p qaqh-contract-export -- export --out {{out}}

# 检测缺失、多余、陈旧产物及契约摘要
sdk-check against:
    cargo run -p qaqh-contract-export -- check --against {{against}}

# 在独立临时目录验证单测、确定性、摘要及可选严格 TS 消费
sdk-verify *args:
    python3 scripts/check-sdk-contract.py {{args}}

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
