# ADR-0004 wire 兼容方向是声明数据，不是隐含 serde 行为

状态: `已采纳（部分实施）` · 决策日期 2026-11-10 · **§待决 已决**：出站引入 `unknown` fallback，入站维持 reject

> **2026-11-10 第二轮**：落地清单前两条（本 ADR 里唯二的现存 fail-open）已在 `main` 落地，
> 实测数据见 §落地清单。其余条目仍未实施 —— 不要把本 ADR 整体读成"已完成"。

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

## 已决（2026-11-10）：出站面引入 `unknown` fallback；入站面维持 reject

原文这一节是"待决"，现由仓库所有者拍板：**出站取 B（`unknown` 变体）**，入站维持既有的 reject。
下面是核实过可行性之后的落地形状，不是愿望清单。

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

### 一条容易踩错的事实（查证 serde 官方 variant-attrs 文档，不靠记忆）

> `#[serde(other)]` … Only allowed on a **unit variant** inside of an **internally tagged or
> adjacently tagged** enum.

⇒ 本仓的枚举分三类，只有前两类能机械套 `other`：

| 类别 | 实测位置 | `unknown` 怎么加 |
|---|---|---|
| internally tagged（`#[serde(tag = "kind"/"type")]`） | `events.rs:52`、`state.rs:220/256/308/342`、`v2/types.rs:20/31/270/660/703`、`command.rs:45` | 加 **unit variant** `Unknown` + `#[serde(other)]`，零手写 impl ✅ |
| adjacently tagged（`tag="kind", content="data"`） | `fact/content.rs:11`、`fact/types.rs:134/625`、`state.rs:113/129` | 同上，但 `Unknown` 必须是 unit variant ⇒ **降级即丢内容**。UI 必须按"详情不可用"设计，不能假装还在 |
| 裸字符串 unit-only enum（只有 `rename_all`，无 `tag`） | `qaqh-memwatch/src/lib.rs:24/51` 的 `ComponentGroup`/`PhaseKind`，以及同类状态枚举 | **serde 不支持 `other`** ⇒ 要么手写 `Deserialize`，要么保持 closed 由前端容错。这一类需逐个定，是本决定唯一不能自动化的部分 |

### 关键澄清：光在 Rust 加 `Unknown` 保护不了老前端

出站失效发生在**生产端**：新 daemon 发 `kind:"super_thing"`，pin 在 v3.0 的前端不认识。
`#[serde(other)]` 只让**反序列化方**（daemon 读自己的旧 DB、以及未来的 daemon）容忍；
老前端的 TS 类型不会因为后端加了变体就自动长出容错。所以 B 的真机制是大厂那种
**按版本降级发射（version-gated emission）**，共四件事：

1. **契约声明**：出站 enum 在 `coverage.json` 带 `variant_policy: "fallback"`，并声明线上保留值
   `"unknown"`；TS / C# / JSON Schema 三个投影都让 `"unknown"` 出现在可取值里。
2. **生产端闸门**：每个新变体带"自哪个 revision 起存在"的元数据；daemon 向**较老的 client session**
   序列化视图时，把对方不可能认识的变体改写成 `unknown`。依据是既有协商面
   （`RingingV2OpenResponse.command_contract_revision` / `session_contract_revision` 已在
   `main.rs:306-307` 握手携带），缺的只是"按 revision 降级"这一步。
3. **前端表现**：`unknown` 必须有确定文案（不是空白/不是崩），否则"容错"与"坏掉"无法区分。
4. **入站不变**：未知判别串继续 reject —— `artifact_tests.rs:77-107` 已把这层锁住。

代价写清：第 2 项是这套里唯一真正复杂的。没做第 2 项之前，第 1+3 项只保证"前端不崩"，
**不保证"老前端看到的是它认识的值"**。所以落地清单把 2 单独列一条，不假装一步到位。

## 落地清单

顺序有讲究：先收**已经存在的 fail-open 盲区**（前两条，不依赖任何新机制），
再做 `unknown` 那套（第 1、4、5 条，属 `contract-refreeze` 类改动，只能走 main / 新线）。

- [x] 收口契约盲区：`PermissionRequestDetails` 纳入导出物（TS + C# + JSON Schema + fixture）
      —— 实测导出物 599 → 602：新增 `types/qaqh/PermissionRequestDetails.ts`、
      `cs/PermissionRequestDetails.cs`、`fixtures/pending-permission-details.json`；
      `coverage.json` 的 `approvals.admin` 现在同时列类型、正文夹具与
      `wire_notes`（错误码表）。`contract_hash`
      `c25b08f1…` → `39256ad9…` ⇒ 分类闸判 `contract-refreeze`（`artifact added ×3`），
      只落 `main`，`docs/api-surface/v3.0` 一个字节没动（切线时由机器重切，见 ADR-0010）。
      JSON Schema 侧顺带产出一条**派生**证据：deserialize 视图对 `deny_unknown_fields`
      自动给出 `additionalProperties: false`，同一份 DTO 的 `level: u8` 在 schema 里是
      `format:"uint8"` + `0..=255`，而经 TS 窄腰的 `cs_backend` 投出的是 `long` —— §决策 1
      要的 compat 声明确实可以派生，不必手抄表（ADR-0008 的靶子又多一例）。
- [x] `v2.rs` 的 `.ok()` 静默降级改为显式失败 —— 落点是新增的
      `ApprovalBodyError`（`crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs`）：
      存储侧取不到才允许降级（`details_unavailable` / `details: null` 保持既有
      fail-closed 语义与 `is_valid()` 权威），形状侧读不通一律带错误码失败：
      `interaction_body_not_json`（inline 正文与 content store 字节都不是 JSON，含原
      `:555`/`:563` 两处）、`permission_details_shape_invalid`（含 `deny_unknown_fields`
      撞上的多余字段）；`is_valid()` 为假走类型化回退 + `permission_details_incomplete`
      的 warn，不吞掉也不至于让可答复的卡片消失。错误码表由 `coverage.json` 的
      `wire_notes` 记账。反证：夹具值域被改成 `category="filesystem"` 后导出器
      直接失败（`approvals fixture is not a body the daemon could emit`），
      即"样例必须是 daemon 真发得出的形状"这条也是机器闸而不是评审意见。
- [ ] 列出**出站 enum 全集**并逐个归入 §已决 的三表之一（internally tagged / adjacently tagged /
      裸字符串 unit-only）；第三类的处置必须逐个定，不能默认它能自动化
- [ ] **`config.load` 的读数路径不做值域校验**（2026-11-10 第二轮实测新增，出站半边）：
      `crates/qaqh-config/src/config.rs:860` 直接接受 `PersistentConfig.auto_compact_threshold`
      （TOML 规范允许 `nan`），而 `ConfigDto.auto_compact_threshold` 是 `f64`；
      `serde_json::to_value` 对非有限浮点是 **成功**并把值塌成 `null`
      （`value/from.rs` 文档 + 断言 `v2_to_value_collapses_non_finite_floats_to_null`）。
      后果：非法配置不报错，而是以 `"autoCompactThreshold": null` 的身份出现在契约面上
      ——**静默换值**，比报错更坏。`PersistentConfig` 自己声明的是 `strict-input`
      （本 ADR 表第 3 行），所以在载入处显式失败才与它声明的类别一致；改法待定，
      因为它同时决定"旧配置文件里的非法值要不要给迁移提示"这一用户可见行为。
- [ ] 契约声明 `variant_policy` + 线上保留值 `"unknown"`，并让 TS / C# / schema 三个投影都可见
      （改导出物 ⇒ 提交必须带 `[contract-refreeze]`，禁入 `LTS-v3.0`）
- [ ] 生产端 revision 闸门：变体带"自哪个 revision 起"的版本戳，向较老 client session 序列化时
      把对方不认识的变体降级为 `unknown`（§已决 第 2 项，本清单里唯一复杂的一条）
- [ ] 全仓扫一遍 wire 面上的 `deny_unknown_fields` 与 `from_*().ok()`，逐条分类
- [ ] IR 上加 `compat` 属性，导出进 `coverage.json`（与 ADR-0008 合并落地；
      `deny_unknown_fields` 一出现即可由 JSON Schema 的 `additionalProperties: false` 反推，
      所以 compat 可以**派生**而非手抄 —— 手抄表正是本 ADR §被否选项里否掉的东西）
- [ ] 守卫加"compat 类不得降级"检查
- [ ] 前端（`../qaqh-electron`）为 `unknown` 定确定文案；没有它，第 4/5 条只是后端自嗨
