# QAQHarness `v4.8`（`P3-6`）执行方案：物理立斩 RingingV2 旧栈 + 纯 V3 单栈收口

> **基线**：`qaqh-backend` `main` @ `v4.7.100`（`c6c59d70`）· `../qaqh-gpui` `main` @ `debd2d1`  
> **目标分支 / Worktree**：`../qaqh-wt-v4.8-demolish-v2` · `refactor/v4.8-demolish-ringing-v2`  
> **最高约束**：《项目宪法 v2》第 1–17 条 · [MUST.md](./MUST.md) · [ADR-0047](../adr/0047-audit-and-journal-consolidation.md)（`已采纳 · 未实施`，本切片落账转为已实施）  
> **定位**：12 月前三天清场第一刀（Day 1）——在 `v4.9` 工具 Trait 化与 `v4.10` `SessionLoop` 解耦之前，先把所有 V1/V2 HTTP/WS/SSE 外皮与 `daemon.json` 物理清零，使 `qaqh-daemon` 成为纯粹的 RingingV3 单栈进程。

---

## 一、明确范围边界（做与不做）

- **本切片必做**：
  1. 整目录物理删除 V2 HTTP/SSE/WS 服务端、V2 客户端、V2 契约导出器与相关测试夹具；
  2. 将 `qaqh-daemon` 的 `run`/`server`/`status`/`stop`/`todo` 全部收敛到 `RingingV3` 本地 IPC 与 `daemon.endpoint`（彻底移除 `daemon.json`）；
  3. 迁移 `qaqh-acp` 到 `V3Client::connect_local`（或剥离其 `qaqh-client` V2 依赖），使主工作区不残存 V2 消费端；
  4. 将尚待 `27.0.0` 再做正式联调的移动端 FFI Crate（`crates/qaqh-ffi`、`crates/qaqh-harmony-napi`）移入根 `Cargo.toml` 的 `exclude` 列表（保留源码，不陪葬构建）；
  5. 落完 [ADR-0047](../adr/0047-audit-and-journal-consolidation.md) 的台账标注与单写者注释；更新 `.github/workflows/{v3-ci.yml,release.yml}` 门禁。
- **本切片不做（留待 `v4.9` / `v4.10`）**：
  - 不顺手删 `qaqh-ringing-contract` 的内部 `TimelineEntry` / `SessionEvent` / `ToolResult` 引用（它们仍被 `SessionLoop`、`SessionOwner`、`v3_bridge/projection` 使用，分别在 `v4.9` 与 `v4.10` 物理清零）；
  - 不改 `session_loop.rs`、`TypedTool::run` 与 SQLite `timeline_blocks` 结构；
  - 不开启移动端或远端 `iroh` 联调（留 `27.0.0`）。

---

## 二、物理删除清单（`git rm -rf`）

1. **服务端 V2 HTTP/SSE/WS 与 TLS 壳**：
   - `crates/qaqh-daemon/src/axum_server.rs` 及 `crates/qaqh-daemon/src/axum_server/` 全目录；
   - `crates/qaqh-daemon/src/tls.rs`（原 LAN HTTP+TLS 监听器产物；远端加密传输在 `27.0.0` 走 `iroh`/QUIC）；
   - `crates/qaqh-runtime/src/ringing/service_methods.rs` 中仅服务于 V2 `/ringing/v2/service/*` 文本分发的残留及对应 `qaqh-client` 对账测试。
2. **协议外皮 & 契约导出器**：
   - `crates/qaqh-ringing/src/v2/` 全目录；
   - `tools/qaqh-contract-export/` 全目录（及 `Cargo.toml` `members` 中对应条目）。
3. **客户端 V2 传输实现**：
   - `crates/qaqh-client/src/{client.rs,v2.rs,v2_stream.rs,sse_decoder.rs,shell_stream.rs,endpoint.rs,projection.rs,service.rs,service_types.rs,session.rs,tls_pin.rs,types.rs}`；
   - `crates/qaqh-client/tests/{lease_renew_timeout.rs,v2_public_api.rs}` 与 `crates/qaqh-client/examples/shell_stream_e2e.rs`；
   - 保留 `crates/qaqh-client/src/{lib.rs,discovery.rs,error.rs,remote_path.rs,v3/}`（剥离 `discovery.rs` 中的 `daemon.json` / `DaemonDiscovery` 读写逻辑，仅保留 `data_dir`、`daemon_executable`、`spawn_daemon_process` 与全局 Tokio runtime 句柄）。

---

## 三、接线改造明细

1. **`Cargo.toml`（根工作区与涉及 crate）**：
   - 根 `Cargo.toml`：`members` 移除 `tools/qaqh-contract-export`，将 `crates/qaqh-ffi` 与 `crates/qaqh-harmony-napi` 移入 `exclude`（注明：`27.0.0` 开启 Mobile 纯 V3 联调时移回）；`version` 推进为 `4.8.0`。
   - `crates/qaqh-daemon/Cargo.toml`：移除 `axum`、`tower`、`tower-http`、`rustls`、`tokio-rustls`、`rcgen`、`http-body-util`、`tokio-tungstenite`；生产依赖移除 `qaqh-client`（仅按需留 `dev-dependencies` 测本地 IPC）。
   - `crates/qaqh-client/Cargo.toml`：移除 `reqwest`、`tokio-tungstenite`、`base64`、`shell-stream` feature 及只为 V2 HTTP/TLS 服务的 `rustls` 分支，成为纯粹轻量的 `V3Client` 库。
2. **`crates/qaqh-daemon/src/{server.rs,main.rs}`**：
   - `server.rs`：删除 TCP/TLS `TcpListener`、`DaemonDiscovery`、`write_discovery`（`daemon.json`）、`axum::serve`、`AppState`、HTTP `shutdown` 等待分支；保留 `acquire_single_instance`、后台周期清理任务（idle unload / goal / handoff / idle shells）及 `RingingV3` 本地 IPC 监听循环（绑定前清理遗留 socket，退出时清理 `daemon.endpoint` 与 `.sock`）。
   - `main.rs`：`status()` 改走 `V3Client::connect_local(..., None, &channel, "qaqh-cli-status", false)` + `SystemRequest::Health`；`stop()` 改走 `V3Client::connect_local` + `SystemRequest::Shutdown`；`todo_cli()` 改走 `V3Client::connect_local` + `SessionRequest::{List, GetTodos}` / `CommandRequest`。
3. **`crates/qaqh-acp/src/lib.rs`**：
   - 将 V2 `Client` / `RingingV2CommandAck` / `contract_events` 替换为 `V3Client::connect_local` + `qaqh-ringing-v3` 强类型事件与命令。
4. **`ADR-0047` 收尾落账**（`已采纳 · 未实施` → `已实施`）：
   - 在 `crates/qaqh-file-tools/src/journal.rs` 与 `crates/qaqh-workspace/src/audit/mod.rs` 锁定义处补注“显式单写者是设计而非遗留（见 ADR-0047）”；
   - 在 [LOCK_AUDIT_REPORT.md](./LOCK_AUDIT_REPORT.md) `L-13`/`L-14` 行标注 `已定案，见 ADR-0047`；将 [ADR-0047](../adr/0047-audit-and-journal-consolidation.md) 状态更新为 `已实施`。
5. **CI 门禁同步（`.github/workflows/`）**：
   - 删除 `release.yml` 中 `qaqh-contract-export` 步骤，以及任何 `nightly` 预置；
   - 在 `v3-ci.yml` 中把 V2 字面扫描升级为**全 `crates/` 硬红线**：`axum_server|RINGING_V2|/ringing/v2|/control/v1|/marketplace/v1|v2_stream` 必须全局 0 命中。

---

## 四、硬验收门禁（全部实测通过方可合并 `main`）

1. **V2 物理清零扫描**：
   ```bash
   ! rg -n 'axum_server|RINGING_V2|/ringing/v2|/control/v1|/marketplace/v1|v2_stream|qaqh-contract-export' crates/ .github/
   ```
2. **精准编译与测试（`-j2`）**：
   - `cargo check -j2 --workspace --all-targets`（全工作区 0 错，确认无遗漏引用方）；
   - `cargo test -j2 -p qaqh-core-vocab -p qaqh-ringing-v3 -p qaqh-v3-transport -p qaqh-client -p qaqh-acp`；
   - `cargo test -j2 -p qaqh-daemon --bin qaqh-daemon v3_server -- --test-threads=1`；
   - `cargo test -j2 -p qaqh-daemon --bin qaqh-daemon v3_bridge -- --test-threads=1`。
3. **跨仓 CLI + GPUI 连通性闭环**：
   - 隔离 `QAQH_DATA_DIR` 下依次执行 `qaqh-daemon run` → `qaqh-daemon status` → `qaqh-gpui --probe` → `qaqh-daemon stop`，全部通过且退出后无残留 `daemon.endpoint` / `.sock` / `daemon.json`。
