# Ringing v2 timeline 切换 Handoff（2026-09-24）

状态：**后端已落地（v2 路由 + v1 硬切）；TUI 侧需要一次 rev bump 才能吃到。**

## 1. 这次修的是什么

`capabilities.timeline = true` 但 `/ringing/v2/sessions/{seed}/timeline` 与
`/timeline/events` **根本没有注册**——TUI 一直走 v1 路径。这是纯 v2 化剩下的最大一条
「说了谎的 capability」。

## 2. 改动

| 面 | 改动 |
|---|---|
| daemon 路由 | `GET /ringing/v2/sessions/{seed}/timeline`、`/timeline/events` 注册（handler 复用，行为不变）；v1 两条路径删除（404） |
| qaqh-client | `TimelineStream` / `get_timeline_page` 路径改 v2；`download_content` 早前已改 v2 |
| webui gateway | timeline snapshot / events 两个 proxy 改指 v2（bootstrap / approvals 仍是 v1） |
| 测试 | 新增 `timeline_v1_routes_are_hard_cut`；既有 timeline 用例全部改走 v2 路径 |

timeline 自己的 wire（`watermark` 分页、`Last-Event-ID`、`TimelinePage`）**没有改**——
它本来就是独立 wire，本次只换路径与归属面。

## 3. 验证

```text
cargo test --workspace -- --test-threads=1              PASS（0 failed）
cargo clippy --workspace --all-targets -- -D warnings   PASS
cargo fmt --all -- --check                              PASS
scripts/v2-smoke.sh <data-root>                         PASS（全阶段）
scripts/v2-content-probe.sh <data-root>                 PASS（#345 专项）
```

**真机（TUI 用本 rev 重建 + 本仓 daemon）**：

| e2e | 结果 |
|---|---|
| `e2e-history.sh`（深分页 / 历史列表） | PASS（6/6 判据） |
| `e2e-new-session.sh` | PASS（4/4） |
| `e2e-restart.sh` | 相位 `connecting → ready c7bbd484 → lost → ready 6049ca9e`（换 daemon 后自愈，epoch 不同） |
| `e2e-alpha1-basic.sh` | PASS ×2（其中一次先出现「信任此目录」权限 modal 未及时应答的 flake，复跑稳定） |

重建后的 TUI 二进制里 `/ringing/v2/sessions/` 存在、`/ringing/v1/sessions/…/timeline`
不再出现。

## 4. TUI 侧要做的（未提交，需要一次 rev bump）

TUI 现在通过本机 `.cargo/config.toml` 的 `paths` 覆盖把 `qaqh-client` /
`qaqh-config-api` 钉在锚点 worktree（`qaqh-backend-anchor` @ `b77c251`），
`scripts/ci-linux.sh` 的 `QAQH_BACKEND_REV` 同源。要吃到 v2 timeline：

1. **补一个 match 臂**（锚点之后 `ControlEvent` 新增了 `DriverChanged`，不补则编译不过）：

   ```rust
   // src/app/mod.rs::handle_control 末尾
   ControlEvent::OperationCompleted { .. } => {}
   // v2 的 driver 席位状态来自 bootstrap（`BootstrapSnapshot.driver`），
   // 这条 v1 双发事件在 TUI 侧无需额外处理。
   ControlEvent::DriverChanged { .. } => {}
   ```

2. **重建锚点 + bump rev**：按 `.cargo/config.toml` 注释里的流程
   （`git -C ../qaqh-backend worktree add --detach ../qaqh-backend-anchor <rev>` →
   改 config 两行 + `scripts/ci-linux.sh` 的 `QAQH_BACKEND_REV`），并给后端 rev 打
   annotated tag（沿用 `tui-ringing-v2-*` 命名，旧 tag 不动）。

本次验证时这两步都做了（临时改动，已还原，未提交 TUI 仓）。

## 5. 仍是 v1 的面（纯 v2 化的剩余清单）

| 端点 | 现状 |
|---|---|
| `POST /ringing/v1/clients/open` | 客户端仍在用（v2 open 已存在） |
| `POST /ringing/v1/leases/renew` | 同上（v2 renew 已存在） |
| `POST/GET /ringing/v1/commands/{id}` | 客户端命令面（v2 命令面已存在） |
| `GET /ringing/v1/sessions/{seed}/bootstrap` | 客户端仍走 v1（v2 bootstrap 已存在） |
| `GET /ringing/v1/sessions/{seed}/approvals` | 只读审批查询，v2 无对应端点 |
| `POST /ringing/v1/service/{method}` | typed service RPC（v2 service 已存在） |
| `GET /ringing/v1/events/{channel}` | v1 三频道 SSE（v2 单流已存在） |

⇒ 客户端（qaqh-client）整体从 v1 迁到 v2 是下一步的主线；迁完才能删掉上表。

## 6. 其它注意

- `docs/spec/2026-09-23-TUI契约测试钩子-spec.md:93` 仍写着 v1 timeline 路径，属
  **陈旧 spec 文本**（未改：spec 是冻结记录，应走修订流程）。
- `handle_timeline_snapshot` / `handle_timeline_events` 的 handler 名与内部注释仍带
  v1 语境（实现已复用），若要彻底去 v1 语义可另开一条命名清理。
