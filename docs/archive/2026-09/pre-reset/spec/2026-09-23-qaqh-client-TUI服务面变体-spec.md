# qaqh-client TUI 服务面变体 spec（2026-09-23）

> 状态：**已实现**。本变更只补 `qaqh-client` 的封闭变体，不新增 wire 方法；
> 服务端方法表和实现早已存在。
>
> 目标：TUI 不再为了 `session.meta` / `plan.*` / `stats.token_usage` / `git.*`
> 自建 HTTP 或绕过 `qaqh-client`。

## 1. QueryRequest（只读）

| 变体 | wire 方法 | 参数 |
|---|---|---|
| `SessionMeta { seed }` | `session.meta` | `{ seed }` |
| `PlanRead { seed }` | `plan.read` | `{ seed }` |
| `PlanContextStats { seed }` | `plan.context_stats` | `{ seed }` |
| `StatsTokenUsage { days }` | `stats.token_usage` | `{ days }` |
| `GitDiff { seed }` | `git.diff` | `{ seed }` |
| `GitBranch { seed }` | `git.branch` | `{ seed }` |
| `GitBranches { seed }` | `git.branches` | `{ seed }` |
| `GitFileDiff { seed, file_path }` | `git.file_diff` | `{ seed, file_path }` |

说明：

- `seed` 作用域方法仍由 daemon 做 lease 归属校验；
- `stats.token_usage.days` 由 daemon 钳制到 `1..=366`；
- 返回值仍为 `serde_json::Value`，与现有 `Client::query` 面一致；本变更不引入
  第二套响应类型。

## 2. ActionRequest（写）

| 变体 | wire 方法 | 参数 |
|---|---|---|
| `GitSwitchBranch { seed, branch, stash }` | `git.switch_branch` | `{ seed, branch, stash }` |
| `GitCommit { seed, message }` | `git.commit` | `{ seed, message }` |

`git.switch_branch` / `git.commit` 是变更操作，必须走 `ActionRequest`；不得塞进
`QueryRequest`。

## 3. 使用示例

```rust
use qaqh_client::{ActionRequest, QueryRequest};

let meta = client
    .query(QueryRequest::SessionMeta { seed: seed.into() })
    .await?;

let plan = client
    .query(QueryRequest::PlanRead { seed: seed.into() })
    .await?;

let stats = client
    .query(QueryRequest::StatsTokenUsage { days: 30 })
    .await?;

let branch = client
    .query(QueryRequest::GitBranch { seed: seed.into() })
    .await?;

client
    .action(ActionRequest::GitSwitchBranch {
        seed: seed.into(),
        branch: "main".into(),
        stash: true,
    })
    .await?;
```

## 4. 回归门

`crates/qaqh-client/src/endpoint.rs` 内已更新：

- `into_parts` 路由映射；
- `all_query_requests` / `all_action_requests` 穷举清单；
- 新增变体参数形状断言；
- 路由总数 `24 -> 34`。

```text
cargo test -p qaqh-client --all-targets
cargo clippy -p qaqh-client --all-targets -- -D warnings
```
