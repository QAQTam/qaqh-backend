# QAQ-Harness Backend

QAQ-Harness 的 Rust 后端 monorepo。daemon 承载多会话 Agent 循环、canonical session
facts、Ringing v2 单流、工具执行、权限/沙箱、compact 和 provider gate。

- Edition 2024
- License MIT
- 状态：beta / RC 前架构清理
- 当前架构基线：[ARCHITECTURE.md](docs/ARCHITECTURE.md)
- 当前代码基线：`2.0.0-beta.3`

RC 前架构清理在 `clean` 分支推进，唯一派工单是
[架构收敛施工单 §10](docs/spec-architecture-convergence.md#10-clean-rc-前砍刀与-crate-减负)。
本轮聚焦删除旧路径、明确状态与存储所有权、crate 职责减负及回归。

## 当前协议面

- 客户端只使用 **Ringing v2**。
- v1 三频道流和 `/ringing/v1/*` 路由已硬切删除。
- 主要路由：
  - `POST /ringing/v2/clients/open`
  - `POST /ringing/v2/leases/renew`
  - `GET /ringing/v2/sessions/{seed}/bootstrap`
  - `GET /ringing/v2/sessions/{seed}/events`
  - `POST /ringing/v2/commands/{channel}`
  - `GET /ringing/v2/sessions/{seed}/timeline`
  - `GET /ringing/v2/content/{content_id}`
  - `POST /ringing/v2/service/{method}`

## 核心分层

```text
qaqh-daemon      axum HTTP/SSE、lease、driver、service
qaqh-runtime     Agent loop、TurnActor、ToolRuntime、RingingHub
qaqh-session     canonical facts、projection、replay、messages.jsonl
qaqh-message     MessageStore、WAL、compact archive watermark
qaqh-gate        OpenAI Chat / Responses / Anthropic / Gemini provider HTTP
qaqh-workspace   typed tools、permission、audit、sandbox integration
qaqh-types       shared types / storage contracts
qaqh-client      Ringing v2 共享传输(TUI / 桌面壳)
```

## 桌面客户端(已拆仓)

Tauri 2 桌面壳 + SolidJS 渲染层（原 `webui/`）已于 **2026-10-07** 抽成独立仓
**`qaqh-desktop-app`**（同级目录 `../qaqh-desktop-app`，含完整 git 历史）。
本仓不再包含前端、也不构建它。

该仓经 path 依赖吃本仓的 `qaqh-client` / `qaqh-types`，并以 sidecar 托管 daemon
（daemon 的权威构建仍在本仓）；渲染层的类型契约 `src/api/qaqh/*.ts` 由本仓
crate 的 `derive(TS)` 导出、但生成动作从那边发起。原浏览器 gateway
(`qaqh-webui-gateway`)已移除，daemon token 不进 webview。

改 `wire` 类型后：去 `../qaqh-desktop-app` 跑 `just ts-export` 并提交那边的生成物。

## 当前事实源

- canonical session facts：`events.jsonl` + commit marker。
- 消息归档：`messages.jsonl`，append-only。
- 压缩：摘要 append 到 `messages.jsonl`，`meta.compact_covered_through_msg_id`
  记录覆盖水位；没有 `compact-context.json`。
- 投影、timeline、snapshot 都是派生数据，不是事实源。

## 构建与验证

```bash
cargo test --workspace -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

真实 daemon 探针：

```bash
QAQH_SMOKE_LEASE_TTL_MS=30000 ./scripts/v2-smoke.sh /path/to/qaqh
./scripts/v2-content-probe.sh /path/to/qaqh
QAQH_CONTENT_PROBE_MODE=permission ./scripts/v2-content-probe.sh /path/to/qaqh
./scripts/v2-compact-probe.sh /path/to/qaqh
```

## 文档

- 当前权威文档：[`docs/current/`](docs/current/)
- 文档入口与规则：[`docs/README.md`](docs/README.md)
- 当前待办：[`docs/current/debug-backlog.md`](docs/current/debug-backlog.md)
- 历史文档：[`docs/archive/`](docs/archive/)

`docs/archive/` 只用于追溯历史，不作为当前实现、接口或排期依据。
