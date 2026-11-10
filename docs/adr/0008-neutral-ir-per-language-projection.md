# ADR-0008 中立 ContractIR + 每语言投影，退役 ts→cs 解析链

状态: `已采纳（第 1 步已落地：schema 预览投影；主重构未实施）` · 决策日期 2026-11-10 ·
**决策人：仓库所有者**（在"同意退役，按中立 IR + 每语言投影重构（工作量大但架构更好）"选项中拍板）

## 背景（已核对的代码事实）

`qaqh-contract-export` 的实际依赖链是：

```
Rust DTO ──ts-rs(12)──▶ types/*.ts ──cs_backend.rs 手写 lexer/parser──▶ cs/*.cs
```

`main.rs:28-30` 的注释直陈这个顺序耦合：

> cs_backend must run after the ts-rs type export: **it parses the generated TypeScript subset**
> and re-encodes the same wire contract as C# sources.

规模（实测）：`cs_backend.rs` **1054 行 / 43.8 KB**，与 `main.rs`（1090 行）同量级；其中
`lex()` + `parse_decl()` 是一整台 TypeScript 子集解析机。

**TypeScript 在本仓被实际当成交换 IR 使用了。**

## 问题：不是"不优雅"，而是可验证的 bug 类

### 1. C# 侧靠一张手抄语义表补回 TS 丢掉的信息

`cs_backend.rs:15-21`：

```rust
/// ts-rs renders every numeric as TS `number`; the contract's only float
/// fields are the auto-compact threshold (f64 in the Rust DTOs), so `number`
/// maps to `long` except for these (type, JSON key) pairs → `double`.
const FLOAT_FIELDS: &[(&str, &str)] = &[
    ("ConfigDto", "autoCompactThreshold"),
    ("ConfigPatch", "autoCompactThreshold"),
];
```

`number → long`，除非出现在这张表里。推论可直接判定：**若有人给某个 wire DTO 新增一个 f64 字段，
C# 投影会静默生成 `long`，小数被截断**；而

- TS 侧是对的（`number`）→ tsc 严格消费者绿；
- `contract_hash` 覆盖的是导出物字节，新字段是"按规则确定性地生成错的 C#" → 金标准会**愉快地把
  错误字节重新冻结**；
- 路由快照与 facade 与此无关。

六道闸门没有一个能看见它。这与 ADR-0004 §发现 的 `PermissionRequestDetails` 是同一形状：
**信息在穿过 TS 这道窄腰时被丢弃，然后由人肉表格补回**。表格不是闸门。

同族问题：`main.rs:199` 的 `.with_large_int("number")` 让 u64 也塌成 JS number，
超出 `MAX_SAFE_INTEGER` 即丢精度 —— 同样是"投影层丢信息、下游靠约定补"。

### 2. 公共面排除策略存在两份

`cs_backend.rs:49-54`：`DaemonDiscovery` / `JsonValue` 的排除，注释写的是
"kept out of the public surface **exactly like the TS index does**" —— 一句"和那边一样"，
即承认自己维护了第二份真相。`ALIAS_TARGET`（`:23-26`）、`CS_KEYWORDS`（`:28-38`）同理：
都是为修补 TS→C# 落差而存在的层，不是契约本身。

### 3. 第三门语言的代价是"再造一台解析机"

本仓已有非 TS 消费者面：`crates/qaqh-ffi`、`crates/qaqh-harmony-napi`。按现状，新增一个语言后端
只有两条路：再解析一次 TS，或回到 Rust DTO 手写。两者都不可接受 ——
这正是 protobuf / FlatBuffers / LSP `protocol.json` 一致反对的形状（**语言后端永不解析另一语言的产物**）。

## 决策

1. 引入 **`ContractIr`**：单一内存模型，承载
   `type / field / 数值与整数位宽 / 可选性与缺省语义 / tagged enum 判别式 / capability 注册项
   / compat 类（ADR-0004）/ variant_policy`。
2. **三个独立投影，互不解析**：`ts`、`cs`、`schema`。`cs_backend` 的 `lex`/`parse_decl` 退役。
3. **数值语义进 IR，不进表格**：f64 就是 `double`、u64 就是显式位宽，`FLOAT_FIELDS` 这类
   补丁表**没有存在必要因而被删除**（这是本 ADR 最直接的功能性收益）。
4. **公共面排除成为 IR 上的一个属性**（`public: bool` 或 `surface: node|browser|internal`），
   由所有投影共同读取 —— 消灭"exactly like the TS index does"。`main.rs:204-206` 的
   `node.ts` / `index.ts` 二分（`DaemonDiscovery` 只在 `node.ts`）就是这条属性的现状形态。
5. **新增 `schema` 投影**：`schemars` 出 JSON Schema 2020-12（每类型一份 + 方法表引用）。
   用途是 ADR-0002 §后果 里欠的**运行期**校验：fixtures 从"Rust 自 round-trip"升级为
   "按冻结 schema 校验"，从而能表达 must-fail 用例（现在只有编译期 tsc 的 `@ts-expect-error`，
   见 `check-sdk-contract.py:106-123`）。**不是 OpenAPI**，理由仍由 ADR-0002 管辖。
6. **registry 与 compat 声明长在 IR 上**：ADR-0003 的路由 registry、ADR-0004 的 compat 类，
   最终以 IR 字段形式存在，投影层只负责语法。三者必须一起设计，否则会出现"IR 之外又多一张表"。

## 被否选项

- **维持 TS 解析链**：否。§问题 1 是静默数据损坏类缺陷，保留它等于长期接受"新增 f64 字段即出错"。
  且它违反宪法第 9 条对"拧巴转换层"的判定。
- **改为手写 C# 类型、只留 TS 生成**：否 —— 那才是真的第二份真相，且 LTS 冻结面无法机器保证两边一致。
- **用 protobuf / tonic 重做 wire**：否。线上已是 JSON（含 `ContentValue`、tagged enum、JSON-in-JSON），
  换序列化 = 直接破坏用户空间，ADR-0001 禁止。本 ADR 只换**内部生成路径**，不动线上字节。
- **引入 openapi-generator**：前提（OpenAPI 为源）已由 ADR-0002 否决。

## 迁移（每一步都有字节级回归可查，禁止一次性替换）

1. **加** `schema` 投影（纯新增，现有输出不变）⇒ 金标准目录变化须走 ADR-0007 的 refreeze 路径。
2. 让 `ContractIr` **从 Rust DTO 直出** C#，与现有 `cs_backend` 输出**并存双跑**，
   逐字节 diff；差异清单即为 TS 窄腰丢掉的语义集合（`FLOAT_FIELDS` 命中项应在其中）。
3. diff 清零（或每一项被判定为"旧输出本身错误"并记录成 ADR 修订）后，**删除 `lex`/`parse_decl`**。
4. `coverage.rs` 手抄表（ADR-0003）在同一窗口迁入 IR，避免 registry 落两次地。

## 后果与代价

- 一次性工作量显著（IR 抽象 + 三投影 + 双跑比对），且触碰冻结面附近代码 ⇒ **全部在 `main` 上做**，
  合入 LTS 只走 ADR-0005 配对测试证明的 cherry-pick。
- 各 DTO crate 需加 `schema` feature（现状已有 `features = ["ts"]` 的先例，见
  `tools/qaqh-contract-export/Cargo.toml` 依赖行），新增一个编译面 ⇒ 本仓构建图变大。
  本机 8 GB 内存下，这类改动**必须交 CI 验证**（rule 10），本地只做 IR 层的单元对拍。
- `GENERATOR_VERSION`（`main.rs:21`，现为 `qaqh-contract-export/0.2.0`）必须 bump 到 0.3.0：
  导出物形状会变，而 `main.rs:116-129` 的所有权检查正好会用版本号拒绝覆盖旧金标准目录 ——
  这条既有机制在此成为迁移的安全带。

## 第 1 步实施记录（2026-11-10）

落地内容：`tools/qaqh-contract-export/src/schema_backend.rs` + CLI `schema-preview --out <dir>`；
`crates/qaqh-memwatch` 加 `schema` feature，7 个导出类型上
`#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]`（与既有 `ts` feature 完全同形，
`dep:schemars` 同样 optional，普通构建面不引入新依赖）。预览产物写在契约树之外
（`schema/qaqh/<Type>.json` + `schema/index.json`），且 `schema-preview` 明确拒绝写入
含 `manifest.json` 的目录 —— 防止手滑把预览灌进金标准。

已实测验证（非推断）：

| 断言 | 证据 |
|---|---|
| 现有导出物**零变化** | 现场 `export` 后与金标准比对：文件集 **599/599 相同**；`contract_hash` 两侧同为 `ead943ac…f651`；`manifest.json` 去掉两个 volatile 字段后全等 |
| schema 投影**确定性** | 连跑两次，8 个文件（7 schema + index）逐字节 SHA256 全等，0 差异 |
| schema 保住了 TS 丢掉的信息 | `Option<u64>`：TS = `number \| null`（无宽度）→ schema = `integer` + **`format: uint64`** + `minimum: 0`；`usize` → `format: uint` |
| 每文件自包含 | `MemorySnapshot.json` 的 `$defs` = ComponentGroup/ComponentMemory/PhaseKind/PhaseSample/ProcessMemory/SessionMemory，无跨文件 `$ref` |

核对过程中另得到三条**强化本文必要性**的证据：

1. **`FLOAT_FIELDS` 那张表确实在独自撑着正确性。** `ConfigDto.autoCompactThreshold`（Rust `f64`）
   在 TS 里是 `number`，在 `cs/ConfigDto.cs` 里是 `double` —— 只对，因为命中了表；表外一切
   `number` 一律出 `long`。新增 f64 字段而不加表项 ⇒ C# 静默出 `long`，全闸门绿。
2. **`u64` 被投影成有符号 `long`。** `cs/ProcessMemory.cs` 把 `Option<u64>` 出成 `long?`；
   `uint64 > i64::MAX` 即溢出。schema 侧的 `format: uint64` 恰好证明**信息在 DTO 里本来就有，
   是 TS 这道窄腰丢的** —— 这条比 §问题 1 的 f64 推演更硬，是已发生的实际降型。
3. **那台 TS 解析机在静默丢错误。** `cargo check -p qaqh-contract-export --all-targets` 出
   **5 条 warning，全部落在 `cs_backend.rs`**（`unused Result` @ `:360`、`:403`、`:415` 的
   `c.next();`；unused variable @ `:298`、`:812`），新增的 `schema_backend.rs` 零 warning。
   词法/语法错误被忽略 = 宪法第 5 条禁止的形状，也是"解析另一语言产物"这条路线的固有代价。

回归：`cargo test -p qaqh-contract-export` = **5 passed / 0 failed**（含
`generated_artifacts_are_deterministic_and_import_closed` —— 与本 ADR 第 1 步"纯新增、现有输出不变"
的断言互相印证；`export_preserves_non_exporter_directories` 则证明导出器不会往非自有目录里灌东西，
`schema-preview` 的 manifest.json 拒写检查与此同构）。

## 落地清单

- [ ] `ContractIr` 数据结构定稿（含 capability registry、compat、variant_policy、surface）
- [x] `schema` 投影（`schemars`）先行落地，纯新增 —— 已落地为 `schema-preview`，pilot = `qaqh-memwatch` 全导出面
- [ ] pilot 扩面：`qaqh-config-api`（把 `autoCompactThreshold` 的 f64 案例纳入）→ `qaqh-types` → `qaqh-ringing*` → `qaqh-runtime::service_methods`
- [ ] 扩面完成后，把 `schema/` 折进 `generate()`（⇒ 走 ADR-0007 refreeze，`GENERATOR_VERSION` 0.2.0 → 0.3.0）
- [ ] C# 改为 IR 直投影，与 `cs_backend` 双跑逐字节 diff
- [ ] 差异清单评审：每条要么被修好，要么成文为"旧输出即错误"
- [ ] 删 `lex` / `parse_decl` / `FLOAT_FIELDS` / `ALIAS_TARGET` / `CS_KEYWORDS`
- [ ] `coverage.rs` 的 `path` 列改为由表全等校验（收口 ADR-0003 r2：registry 已存在，缺的是控制面与反向覆盖）
- [ ] `GENERATOR_VERSION` → 0.3.0，重切金标准（走 refreeze 流程）
