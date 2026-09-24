# MCP 资源清单退出历史 Handoff（2026-09-24）

状态：**已落地**。这是 `docs/plan/2026-09-24-p6-上下文结构解耦设计输入-plan.md`
§4 设计输入 **C** 的第一刀（见 issue #339）。

## 1. 问题

MCP 资源清单被当成「对话事实」注入历史：

- `resource_env_block()` 生成清单摘要，**封顶 20 条**，超出打截断提示；
- 作为 **trailing developer 消息**注入 `messages.jsonl`（`engine_turn.rs` 回合边界）；
- 内容比对门控（变了才注入）——但这恰恰意味着**每次清单变化都会追加一条历史**。

而模型本来就有更好的路径：聚合工具 `mcp` 的 `list_resources` 读的是**同一个本地缓存**
（`conn.cached_resources()`），**无 20 条上限**，且不占历史。

> 注入的是一份「被截断的、可能过期的、占历史的」副本；按需查能拿到完整且实时的。

## 2. 修法

### 2.1 注入改为 opt-in（默认关闭）

- `qaqh_types::PersistentMcpConfig.inject_resource_env_block: Option<bool>`（持久层）。
- `qaqh_config::McpConfig.inject_resource_env_block: bool`，`#[serde(default)]` → **false**。
- `qaqh_mcp::resource_env_block_with` 在 `enabled && inject_resource_env_block`
  两个条件都成立时才产出块；否则 `None` → runtime 的
  `sync_mcp_resource_injection` 直接 return，**不进历史**。
- 代码路径与测试保留（opt-in 用于调试/兼容）。

### 2.2 存在性告知改走 tools 数组

`aggregate_entry(cfg, timeout)` 的 description 改为动态：

- 零 server：`… No MCP servers are configured. Call list_servers to confirm, or configure [mcp.servers] in config.toml.`
- 有 server：`… N MCP server(s) configured: a, b, c. Call list_resources to discover resources and URI templates.`

server 名来自 `BTreeMap`（有序），同一配置下**逐字节稳定**；description 随
`tools_hash` 变化，**不写进 `messages.jsonl`**，所以 MCP 配置变化不再打断
消息前缀。

## 3. 验证证据

```text
cargo test --workspace -- --test-threads=1              PASS（138 suites，0 failed）
cargo clippy --workspace --all-targets -- -D warnings    PASS
cargo fmt --all -- --check                               PASS
scripts/v2-smoke.sh                                      PASS
```

新增/调整用例：

```text
qaqh-config  mcp_config::resource_env_block_injection_is_opt_in
qaqh-config  mcp_config::absent_section_defaults_to_disabled（补默认 off 断言）
qaqh-mcp     resources::env_block_off_by_default_even_with_cached_resources
qaqh-mcp     resources::aggregate_description_carries_configured_servers
qaqh-mcp     http_transport::http_resource_read（改走 list_resources + 断言注入为 None）
qaqh-runtime mcp_env_block（注入路径本身，夹具显式打开开关）
```

## 4. 顺手修的冒烟脚本脆弱点

`scripts/v2-smoke.sh` 的 lease TTL 之前硬编码 6000ms。当宿主被其它 release
构建打满（本次实测 load 21.6 / 12 核）时，两个续期点之间的阶段会超过 6s，
`driver release` 阶段返回 `lease_required` → 冒烟假红。

修法：`QAQH_TEST_LEASE_TTL_MS="${QAQH_SMOKE_LEASE_TTL_MS:-6000}"`——默认不变，
高负载时用 `QAQH_SMOKE_LEASE_TTL_MS=30000` 跑。

## 5. 仍未完成（alpha 迭代清单）

1. **P6 设计输入 A（前缀 Segment 分区）**：~~待做~~ **已复核不成立**；
   生产路径的 `push_system` 只在会话建立时调用，运行期注入全部走 trailing，
   以 `debug_assert` 固化护栏即可。
2. **P6 设计输入 B（compact 进 canonical log）**：~~`compact-context.json` 仍是
   第二真源~~ **路线 1 已落地（2026-09-25）**：摘要 append 进 `messages.jsonl`，
   meta 记覆盖水位；`build_context_for_gate` 的整段 clone 仍属后续内存优化。
3. **driver 侧剩余**：回收延迟（3s 巡检）、`not_eligible`/优先级、
   `driver_epoch` 未进 command fingerprint、workspace 命令 gate 集合。
4. **崩溃路径 fence 轮转**：`ToolLedger` 的 `Drop` 只覆盖有序退出；SIGKILL
   仍要等 TTL。
5. **#337 三通道收敛**（P1）与 **#323 缺口 1/3**。

## 6. 接手注意

- `inject_resource_env_block` 默认 **false** 是刻意的：不要因为「模型不知道
  有哪些资源」就把它打开——先看 `mcp` 工具的 description 是否已带上 server
  名单，再考虑 `list_resources` 的调用引导。
- `resource_env_block` 与 `mcp list_resources` 是**两套独立渲染**（前者单行紧凑
  + 20 条封顶，后者 `## server` 分节 + 无上限），不要假设它们复用。
- 新增 `McpConfig` 字段会同时打穿 `PersistentMcpConfig`、`map_mcp_config`、
  `into_persistent` 与十余处测试字面量——这是已知的面宽。
