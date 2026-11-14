# 4.0.0 从新：契约面重建指引

所有者指令：冻结面产物与**闸门全部拆除**，4.0.0 从新开线。本文件记录从零重建契约面的
入口。命令均摘自仓内既有文档与 `justfile`，未新增命令。

## 已拆除

**冻结面产物**（上一步）：`docs/api-surface/`（`v3.0/`、`v3.1/`、
`snapshots/frozen-locks.json`）、`qaqh-cs-app/contract/`、
`qaqh-cs-app/tests/Qaqh.Sdk.Tests/Fixtures/`。

**闸门**（本步）：

- `.github/workflows/{lts-api-guard,surface-policy,sdk-contract}.yml`
- `scripts/{check-lts-api-surface,check-surface-change-policy,check-frozen-ledger,check-sdk-contract}.py`
- `.github/CODEOWNERS`、`.gitattributes` 的 `docs/api-surface/**` 条目
- `justfile` 的 `sdk-verify` recipe
- `qaqh-cs-app/tests/Qaqh.Sdk.Tests/ContractFixtureTests.cs` 与 csproj 的 `Fixtures\**\*` 项

产物删除走回收站，可回滚：`D:\Projects\qaqh-backend\.qaqh\trash\` 下
`docs__api-surface.*`、`cs-app__contract.*`、`cs-app__Fixtures.*`。

## 从零重建（4.0.0）

契约唯一真相源是 Rust wire DTO（ADR-0002），产物由 `qaqh-contract-export` 机器生成。
从新开线**不需要** `--rebaseline` / `--pin-frozen`（那两个入口随闸门一起拆了），
直接导出到一个新目录即可。

```bash
# 后端：导出到新目录（目录须由 exporter 管理或尚不存在）
cargo run -p qaqh-contract-export -- export --out docs/api-surface/v4.0 --release
#   等价 just: just sdk-export docs/api-surface/v4.0

# 校验产物自洽（exporter 自带 check，不依赖已拆的 guard 脚本）
cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.0 --release
#   等价 just: just sdk-check docs/api-surface/v4.0

# 导出 provenance（可选，见 docs/lts-v3.0-maintenance.md 的再生成命令）
QAQH_SOURCE_REVISION=$(git rev-parse HEAD) QAQH_SOURCE_DIRTY=0 \
  cargo run -p qaqh-contract-export -- export --out <tmp> --release

# C# 侧同步（../qaqh-cs-app）
#   backend: cargo build -p qaqh-contract-export
#   cs-app:  just sync-contract     # = export --out contract/ + 复制
#              cs/*.cs → src/Qaqh.Sdk/Generated
#   校验:    dotnet test
```

## 铁律

- 契约产物**只能由机器写**（`export --out`）；手改等同作弊（项目宪法第 16 条）。
- 路由只能在两张表里声明（`RINGING_V2_HTTP_ROUTES` / `CONTROL_PLANE_ROUTES`，
  宪法第 16 条）；`crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs` 有说明。
- 决策留痕见 `docs/adr/`；引用 ADR 时须看状态字段，勿把「已采纳(未实施)」当成「已完成」。

## 仍存在的历史悬空引用（未删，等后续步骤）

以下文档写于 v3.x LTS 线，仍引用已拆的闸门/路径，属于历史记录，未随本次删除：

- `AGENTS.md`（项目宪法）第 15/16/18/20 条
- `docs/adr/*`（尤其 0007 / 0011）、`docs/handover-2026-11-10.md`
- `docs/lts-v3.0-maintenance.md`、`docs/lts-v3.1-maintenance.md`

`tools/qaqh-contract-export/` 保留为唯一生成器，其 `export --out` / `check --against`
均可用。原 `--rebaseline` / `--pin-frozen` 属于已删的 `scripts/check-lts-api-surface.py`
（不属于 exporter），从新开线不再需要这两个入口。
