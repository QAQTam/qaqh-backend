# buglist（2026-09-15）— `--features ts` 编译不过：G1 新类型没跟上派生

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @e39d7f7` / `verified` / `wontfix`。
>
> 本条目**未单开 report**（证据自足并逐条内联于下）。
> 发现来源：本轮做深翻页时给 `qaqh-domain::TimelineTurn` 加 `ts` 属性，顺手验了一下
> `--features ts`，发现它**本来就不编译**。

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-15-06 | `fixed @e39d7f7` | `cargo check -p qaqh-domain --features ts` 编译失败（3 个 `E0277`）。G1 给 `ConversationState` 派生了 `TS`，但它的载荷类型 `TurnData` / `RoundData` / `RoundBlock` / `ToolCallDef` / `ToolResultDef` / `FileSnapshotInfo` **一个都没派生** |

## 事实与证据

修复前（`HEAD` 实测）：

```
$ cargo check -p qaqh-domain --features ts
error[E0277]: the trait bound `TurnData: TS` is not satisfied
   --> crates/qaqh-domain/src/state.rs:45:16
    |
 45 |     pub turns: Vec<TurnData>,
```

失败点：`state.rs:45`（G1 新增的 `ConversationState.turns`）→ `timeline.rs:404`（`TurnData`）。

**为什么这件事重要**（不是「一个可选特性坏了」）：

1. **文档与代码相反**。契约文档
   `docs/spec/2026-09-15-前端契约与client-API稳定性-spec.md` 的 G1 节写着：
   「`ts` feature 覆盖新类型，**web 端可直接生成 TS 类型**」。该断言在本条目修复前
   **是假的**——web 端拿不到任何 TS 类型，因为特性根本编译不过。
2. **它是「上一轮改动漏了配套」的典型**：G1 落地时给新类型加了 ts feature 与
   `ts(export, export_to = "qaqh/")`，但只加在顶层聚合类型上，载荷类型没跟上。
   这种漏在**默认特性下完全不可见**（`ts` 是可选特性，CI/日常构建都不开）。
3. **三端共用**是这里的设计前提：winui 吃 Rust 类型、web 走 ts-rs 生成、TUI 直接依赖
   crate。ts 坏掉等于 web 端的路断了，而且**没有任何构建会红**。

## 处置

给缺失的 6 个类型补上与同文件既有类型一致的派生：

```rust
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
```

覆盖：`ToolCallDef`、`ToolResultDef`、`RoundData`、`TurnData`、`RoundBlock`、
`FileSnapshotInfo`。

**验证**（逐个 crate 实测，均为修复后才为 0）：

```
qaqh-domain      0 个错误
qaqh-types       0 个错误
qaqh-ringing     0 个错误
```

即全仓**三个**声明了 `ts` 特性的 crate 都能在 `--features ts` 下编译。

**验证边界（如实说明）**：只验到「特性可编译」。**没有**验证 ts-rs 导出产物的内容
（探针里 `Type::decl()` 的签名在 v12 需要传 `Config`，写错了没再纠缠）。
要钉住「导出的 TS 长什么样」，需要一条真正的导出测试——本项目当前**没有**
（`grep -rn "export_bindings\|::export()"` 零命中）。这是一处已知的覆盖缺口，
不在本条目范围内。

## 附带发现（非本条目缺陷，记在此备查）

`qaqh-mcp --test lifecycle::crash_marks_disconnected_then_reconnects` 是**flaky**：
全仓 `cargo test --workspace` 并发跑时两次都红，两次单跑都绿（`cargo test -p qaqh-mcp
--test lifecycle crash_marks`）。子进程/时序类，与本次改动无关。未定性、未修。
