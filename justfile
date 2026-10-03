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

# ── webUI 渲染层（Tauri 桌面壳的构建产物）──────────

# 构建 webui renderer:安装依赖 + 静态检查 + 单测 + vite build。
# desktop-build 复用此 recipe;产物 out/renderer 由 tauri.conf.json 消费。
webui-build:
    cd webui && bun install --frozen-lockfile && bun run typecheck && bun run test && bun run build

# ── 前端类型契约（ts-rs 单一真相）──────────────────
#
# webui/src/api/ 是**生成物**：crates/qaqh-{types,domain,ringing,session,config-api}
# 上的 `derive(TS)` 集中导出到此（`export_to = "qaqh/"` 不变，靠 TS_RS_EXPORT_DIR
# 收口，避免各 crate 散落 bindings/）。前端一律 import 这里，不再手抄 wire 形状。
#
# 注意跨 crate 撞名：qaqh-session 的 ActivityState/ContentRef/ToolError/
# InteractionKind/InterAgentDelivery 与 domain/types 同名但**不同形状**，已用
# `#[ts(rename = "Session…")]` 区分——集中目录里同名即静默覆盖。
#
# TS_RS_LARGE_INT=number 是必须的：ts-rs 默认把 u64/i64 映射成 `bigint`,
# 而它们在 JSON 线上就是 number（生成物里那些 ts(as = "u32") 就是在绕这件事）。
# 导出发生在 `cargo test` 的 export_bindings_* 测试里,所以只能跑测试生成。

[unix]
ts-export:
    TS_RS_EXPORT_DIR="{{justfile_directory()}}/webui/src/api" TS_RS_LARGE_INT=number cargo test -p qaqh-types -p qaqh-domain -p qaqh-ringing -p qaqh-session -p qaqh-config-api --features qaqh-types/ts,qaqh-domain/ts,qaqh-ringing/ts,qaqh-session/ts,qaqh-config-api/ts

[windows]
ts-export:
    $env:TS_RS_EXPORT_DIR="{{justfile_directory()}}/webui/src/api"; $env:TS_RS_LARGE_INT="number"; cargo test -p qaqh-types -p qaqh-domain -p qaqh-ringing -p qaqh-session -p qaqh-config-api --features qaqh-types/ts,qaqh-domain/ts,qaqh-ringing/ts,qaqh-session/ts,qaqh-config-api/ts

# 生成物落后于 Rust 真相则失败——wire 类型改完忘了跑 ts-export 的兜底。
# 退出码非零时看 `git diff webui/src/api` 就是漂移清单。
ts-check: ts-export
    git diff --exit-code webui/src/api

# ── 桌面壳（Tauri,webui-tauri 计划）────────────────

# 把 daemon 构建产物放置为 Tauri sidecar（目标三元组命名,带存在性断言）。
# mode: debug | release
[unix]
[windows]
place-sidecar mode="debug":
    @pwsh -NoLogo -File scripts/place-sidecar.ps1 {{mode}}

# 桌面开发:构建 daemon(debug)→ 放置 sidecar → bun tauri dev(对真实 daemon 走通)。
[unix]
[windows]
desktop-dev:
    cargo build -p qaqh-daemon
    just place-sidecar debug
    cd webui && bun install --frozen-lockfile && bun tauri dev

# 桌面自包含安装包(C1):web 产物 + daemon release + sidecar + tauri build。
[unix]
[windows]
desktop-build: webui-build
    cargo build --release -p qaqh-daemon
    just place-sidecar release
    cd webui && bun tauri build

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
