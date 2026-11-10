# ADR-0004 wire 兼容方向是声明数据，不是隐含 serde 行为

状态: `已采纳（未实施）` · 决策日期 2026-11-10 · 含一个待决子问题（见 §待决）

## 背景（已核对的代码事实）

`deny_unknown_fields` 在本仓**同时存在于两种性质的结构上**，且无任何成文区分：

| 位置 | 结构 | 性质 | 现状 |
|---|---|---|---|
| `crates/qaqh-ringing/src/**` | `RingingV2CommandEnvelope`（`v2/types.rs:49`）等全部 wire 面 | 前端可见 | **0 处** `deny_unknown_fields`（宽松） |
| `crates/qaqh-ringing-contract/src/state.rs:383` | `PermissionRequestDetails` | 前端可见（审批 UI） | **有** `deny_unknown_fields`（严格） |
| `crates/qaqh-types/src/config.rs:8` | `PersistentConfig` | 用户磁盘上的 toml | **有**，且注释写明"旧端点族键一律解析失败并提示重新配置——不做静默兼容" |
| `crates/qaqh-file-tools/**`、`qaqh-process-tools/**` | 工具入参 | 模型↔工具，非前端 | **大量** `deny_unknown_fields`（严格，合理） |

前三行才是本 ADR 的范围：**同一个"字段未知怎么办"的问题，在三个都属用户空间的面上有三种答案，
而唯一的记录是散落的注释**。

## 发现：一个真实的契约盲区，且它 fail-open

核对 `PermissionRequestDetails` 时确认两件事：

1. **它不在契约导出物里。** `docs/api-surface/v3.0/` 下无 `cs/PermissionRequestDetails.cs`、无
   `types/qaqh/PermissionRequestDetails.ts`，`coverage.json` / `index.ts` / `manifest.json`
   三处全文 grep 均无该符号。→ 它的字段集变化**不会让 `contract_hash` 变，也不会让 `check-lts-api-surface.py` 红**。
2. **解析失败被静默吞掉。** `crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:569-571`：

   ```rust
   fn permission_request_details(body: Option<serde_json::Value>) -> Option<PermissionRequestDetails> {
       body.and_then(|body| serde_json::from_value::<PermissionRequestDetails>(body).ok())
           .filter(PermissionRequestDetails::is_valid)
   }
   ```

   `.ok()` ⇒ 形状不符 = `None`，不是错误。加上该结构带 `deny_unknown_fields`，
   **多一个字段就足以让审批详情整块消失**，而接口仍返回 200。

组合后果：审批 UI 的载荷形状是**前端可见面**，却同时具备"守卫看不见"和"坏了不报错"两种性质。
这正是 ADR-0003 里 `.nest()` 漏报所代表的同一类失效 —— 只是它发生在 JSON-in-JSON 边界上，
连正则都无从扫到。宪法第 5 条在此直接适用：**这里该改的是代码（把形状纳入契约、把失败暴露出来），
而不是写个兜底让它"看起来能工作"。**

## 决策

1. **兼容类别成为声明数据**，每个用户空间可见的 DTO 必须带一个显式标记，并导出进契约
   （落在 ADR-0008 的 IR 字段上，由 `coverage.json` 每条 capability 透传）：

   | compat 类 | 语义 | 适用 |
   |---|---|---|
   | `additive-tolerant` | 未知字段必须忽略；新增可选字段属非破坏变更 | RingingV2 wire DTO（现状已如此，但要**写明并锁死**） |
   | `strict-input` | 未知字段拒绝 | 工具入参、配置文件（`qaqh-file-tools`、`PersistentConfig`） |
   | `closed-set` | enum 变体集合封闭，未知变体必须报错或走 fallback（见待决） | `#[serde(tag = …)]` 的 wire enum |

2. **禁止在 `additive-tolerant` 结构上出现 `deny_unknown_fields`**，由守卫检查而非靠评审。
3. **JSON-in-JSON / `serde_json::Value` 边界一律纳入契约**：凡是穿过 HTTP 面的 blob，其内部
   形状必须有对应的导出类型（如 `PermissionRequestDetails` → 进 `types/`+`cs/`+`coverage.json`）。
   不接受"它是 Value 所以不用管"。
4. **解析失败不得静默降级**：wire 面上的 `from_value(...).ok()` / `from_slice(...).ok()` 视为代码
   错误，须改为带错误码的显式失败或类型化回退，并保留 `is_valid()` 之类的校验语义。
5. 守卫新增一条规则：**冻结线上 compat 类不得降级**（`additive-tolerant` → `strict-input`
   在 LTS 上属破坏性变更，与删字段同级）。

## 被否选项

- **"整个仓库统一宽松"**：否。工具入参（`qaqh-file-tools` / `qaqh-process-tools`）严格是**优点** ——
  模型幻觉出一个不存在的参数必须报错，不能静默接受。一刀切会把已对的半边改坏。
- **"整个仓库统一严格"**：否，且更危险 —— 那会把"后端加一个可选字段"升级为跨线破坏事件，
  直接违背 ADR-0001 的"后端可随便改"前提。
- **靠 `#[serde(default)]` 兜住一切**：否。默认值掩盖形状漂移，是宪法第 5 条点名的防御性补丁。
- **把 compat 类写进注释/文档**：已被现实否证 —— `PersistentConfig` 的兼容策略就写在
  `config.rs:8` 的注释里，机器读不到，因此无法成为闸门。

## 待决（需你拍板，不可代答）

**先记录一条实测到的既成事实**（`tools/qaqh-contract-export/src/artifact_tests.rs:77-107`，
`wire_decoders_reject_invalid_discriminants`，本次跑过 = 5 passed）：

- **入站面事实上已经是"reject"**：`CommandTarget` / `RingingCommand` 等是闭合的
  `#[serde(tag = …)]` enum（`v2/types.rs:20/31`），未知判别串 ⇒ serde `Err` ⇒ 请求被拒；
  测试里 `"state":"finished"` 缺字段、`open.version = 99`、`contract_revision` 不匹配
  都断言为 `is_err()`。同族里 `"target":{"kind":"global"}` 能被 serde 接受、由
  `validate()` 拒掉 —— 说明**判别式靠 serde 闭合、语义靠 validate**，两层都 fail-closed。
- 测试**未覆盖**的是"未知 enum 变体串"这一具体形状（它只测了缺字段与错值）。

所以真正待决的不是"要不要 reject"，而是**出站面**：daemon 发出的事件流/视图里出现
新增变体时，pin 在 v3.0 的前端应当怎么办 —— 那是 TS 运行时（非 serde）的解析行为，
本仓现有闸门完全管不到（见 ADR-0005）。

`closed-set` wire enum 遇到**对端不认识的新变体**时：

- **A. reject**：报错。与现有反向测试同源（`scripts/check-sdk-contract.py:112-117` 的
  `@ts-expect-error` 正依赖"账户状态/activity state 是闭合 enum"才成立），类型可判定性强；
  但老前端连上新 daemon 的新变体 = 该次交互直接失败。
- **B. fallback 变体**（`unknown`）：老前端不崩，但会把未知语义降级为"不知道"，
  UI 必须为它设计占位表现，且 TS 类型退化为开放并集。

本 ADR 不替你选。既然入站面已由闭合 serde enum 事实上取了 **A**，剩下真正要拍的只剩一件事：
**出站面是否取 B** —— 老客户端不该因为收到不认识的事件变体而整条流失效，但代价是 TS 类型退化为
开放并集、且必须为 `unknown` 设计 UI 占位。确认后才能在 `coverage.json` 里落 `variant_policy` 字段。

## 落地清单

- [ ] IR 上加 `compat` 属性，导出进 `coverage.json`（依赖 ADR-0008 的数据结构，可先手写最小版）
- [ ] `PermissionRequestDetails` 纳入导出物；补 TS/C# 类型与 fixture
- [ ] `v2.rs:569-571` 及 `ContentValue::Ref` 分支（同文件 `:563`）的 `.ok()` 静默降级改为显式失败
- [ ] 全仓扫一遍 wire 面上的 `deny_unknown_fields` 与 `from_*().ok()`，逐条分类
- [ ] 守卫加"compat 类不得降级"检查
- [ ] 待决项确认后，为每个 wire enum 落 `variant_policy`
