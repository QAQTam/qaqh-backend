# 项目长期记忆 — qaqh-backend (QAQ-Harness)

## 项目定位
AI 编码代理的跨平台 Rust 后端核心。单实例常驻 daemon 承载多会话对话循环、LLM 网关、
19 个内置工具、Agent Skills、子代理隔离执行。桌面壳(WinUI3)/TUI/Web 在**独立仓库**，
统一经自研 **Ringing V1** HTTP/SSE 协议接入——daemon 是唯一协议面，不存在第二套前端协议。

- 规模：16 workspace crate / 309 个 .rs / ~120k 行；Edition 2024；MIT；单一贡献者 QAQTam。
- 复杂度集中：`qaqh-runtime`(37.4k) + `qaqh-workspace`(27.6k) ≈ 全仓 54%，是理解成本的主要来源。
- 装配方向：`qaqh-types` 为根 → `qaqh-runtime` 为装配层(依赖 13 个 crate) → `qaqh-daemon` 为入口。无环。

## 必知的环境坑（每次跑命令前）
- shell 里 `HOME` 为空 → cargo 报 `could not create home directory '/root/.rustup'`。
  必须先：`export HOME=/home/qaqtamsy RUSTUP_HOME=/home/qaqtamsy/.rustup CARGO_HOME=/home/qaqtamsy/.cargo`
- `/tmp` 是 **10M tmpfs**。`mktemp -d` 落 /tmp 会 `StorageFull`，全量测试大面积假红。
  测试数据根放真实磁盘：`QAQH_DATA_DIR=/home/qaqtamsy/.qaqh-testrun TMPDIR=同目录`
- `target/` 已 25G，注意磁盘。

## 质量基线（2026-09-19 @565286c 实测）
- `cargo check --workspace` 0 error；`cargo clippy --workspace --all-targets -- -D warnings` 0 warning。
- 全量测试仅 1 条失败：`tool_outbox_locking::concurrent_sessions_scale_end_to_end`，
  墙钟断言 128ms vs 实测 157ms 的 flaky，非功能回归。
- 云端 `.cnb.yml` **不跑** Rust test/clippy（成本纪律），质量门禁全在本地 recipe 链。

## 项目约定
- 文档体系极强，按 `docs/{buglist|report|spec|plan|handoff|todo}/{yyyy-mm-dd}-{标题}-{类型}.md` 命名。
  `docs/report/TEMPLATE.md` 是报告硬性结构（元信息/结论摘要/分析链/发现详情/次要观察/
  不确定性与未验证假设/产物清单/排期/附录），写报告前必须先读。
- 证据等级 E1 实测 / E2 代码实证 / E3 静态推断，必须标注，禁止把推断写成实测。
- 全仓 clippy deny `unwrap_used` + `string_slice`。
- 版本真源 `version.txt`（`just sync-version` 同步到 Cargo.toml/package.json）；
  但 `QAQH_USER_AGENT` 在 `qaqh-types/src/platform.rs` 手工维护，两处可能漂移。
- 开发流水线会用 codex-cli 并行派发批次修 bug，清单里每条带 行号/现状/动作/验收命令，
  勾选必须补 commit 或验收输出原文。复盘时明确记录「实测不成立」「HEAD 不成立」等证伪结论。

## 当前在飞（2026-09-19）
- 分支 `fix/level4-exec-net-permission`：权限语义定案为 L3 审批 Exec/Net、L4 显式 bypass。
- 下一阶段是 **workspace v2**：一个 typed output 派生 模型投影/展示投影/资源 summary/
  事件/service 响应/TS 类型，todo 为首个试点。分 W0–W5 六阶段，目前文档先行、实现未开工。
- 已知后续项 N-8：Level 4 下 exec 无沙箱，等 Codex 沙箱移植。
