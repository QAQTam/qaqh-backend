# ADR-0010 换锁：v3.1 线的待拍板取值与攒着的 refreeze 清单

状态: `提议中`（§待决 2 **已决：暂不切线**；§待决 1/3 仍等人拍板） · 起草日期 2026-11-10 · 起草人 QAQ agent

> 本 ADR **不定取值**，它把"已经攒在 `main` 上的 refreeze 差额"逐条记账，
> 并把手册 §8（换锁流程）里那几个必须由人定的空填成可勾的选择题。
> 引用本 ADR 时注意状态：**提议中 ≠ 已实施**。

## 背景（已核对事实）

1. `main` 上刚落的一条 `contract-refreeze`（ADR-0004 落地清单前两条，2026-11-10 第二轮）：
   - 导出物 `599 → 602`（含 `manifest.json`，不含 `snapshots/`）：新增
     `types/qaqh/PermissionRequestDetails.ts`、`cs/PermissionRequestDetails.cs`、
     `fixtures/pending-permission-details.json`；改动 `coverage.json`、`index.ts`、
     `fixtures/approvals-pending.json`。
   - `contract_hash`：`c25b08f1265ca2992a91417afe9b11fc3a381e2b1b6950d9cca8f9e4b834fc71`
     → `39256ad9bd0713fe76e65df345891d1f538e9d701fd6dbe075e421a2f420b54c`（本机实测）。
   - `docs/api-surface/v3.0/` **一个字节没动**：那是 `LTS-v3.0` 持有的锁。
2. 因此 `cargo run -p qaqh-contract-export -- check --against docs/api-surface/v3.0`
   在 `main` 上**必红**（`artifact file set differs: 602 vs 599 files; only-in-first=[3 条]`）。
   这条红是 ADR-0007 §决策 4 设计出来的：`surface-policy.yml:78-80` 的
   `continue-on-error: ${{ target != 'LTS-v3.0' }}` 让它在 main 上只报告、在 LTS 上硬拦。
   **不得**为了让它变绿而重切或手改 `docs/api-surface/v3.0`（规则 16）。
5. 但"不许重切"这句话原本**没有机器闸**：分类闸的判据是"改动落点是否落在
   `docs/api-surface/**`"，而本轮这条 refreeze 恰恰不动金标准 —— 实测它对 `46e6dc76`
   输出`未触碰冻结金标准，规则不适用`（exit 0）。于是 main 上唯一看得见契约漂移的一步
   是 report-only 的，"重切一下它就绿"没有东西拦。这一条由 **ADR-0011** 补上
   （锁身份登记 + 守卫硬比 tree OID，`--skip-export` 下也跑）。切 `LTS-v3.1` 时
   `--pin-frozen` 必须与重切金标准落在**同一个提交**（手册 §8 第 3 步已写）。
6. **切线前新得到的量化输入**（`ir-diff`，见 ADR-0008 §第 2 步（量）实施记录）：IR 覆盖
   8/236 型（3.4%）；被覆盖的部分**属性与判别式集合 0 丢失**，但整数属性 38/77 是
   宽度/符号位降级（`ulong→long` 36×、`byte→long` 1×、`uint→long` 1×），全表面 192 个
   整数属性里**无符号 CLR 0 个**、`[JsonUnmappedMemberHandling]` 0 处。
   ⇒ 我上一轮那句"现在就切会把 possibly-wrong 的 C# 冻住"要收窄成：**冻住的不是正在算错的
   字节，而是"精度与 compat 声明继续无人看守"** —— 其余 228 个类型现在连量都量不到。
   所以 §待决 2 攒着的清单要加一项 **IR 扩面**（至少到 `qaqh-ringing*` + `qaqh-config-api`）。
3. revision 的机制事实（决定"要不要 bump"之前必须看清它到底管什么）：
   - `crates/qaqh-ringing/src/v2/types.rs:39/44`：
     `COMMAND_CONTRACT_REVISION = "ringing-commands-7"`、`SESSION_CONTRACT_REVISION = "ringing-session-2"`。
   - `handle_open_v2`（同文件 `axum_impl/v2.rs:56-69`）在**分配 lease 之前**做严格相等校验，
     不等 ⇒ `426 unsupported_contract_revision`；`RingingV2OpenRequest::validate()`
     （`v2/types.rs:373-377`）与 `RingingV2CommandEnvelope::validate()`（`:119-121`）同律；
     客户端在 `crates/qaqh-client/src/v2.rs:192` 还对着 bootstrap 复查一次。
   - ⇒ bump 是**运行时互斥开关**，不是版本号装饰：一旦 bump，pin 在 v3.0 的客户端在 open
     阶段就再也连不上 v3.1 daemon。反过来，不 bump 时新旧客户端与 daemon 之间没有任何
     机器手段区分"我拿的是哪把锁"，区分只能靠 tag + `contract_hash`（ADR-0006 未落地）。
4. 本轮 `main` 上的改动对前端的可见性：
   - 新增导出类型：纯增量，旧客户端不认识它也能继续跑（它只是没人 `import` 的一段 TS）。
   - `GET /ringing/v2/sessions/{id}/approvals` 的**失败路径**变了：正文存在却读不成 JSON、
     或 JSON 不合 `PermissionRequestDetails` 形状时，从"200 + 详情为 null"改为
     "500 + 错误码"（`interaction_body_not_json` / `permission_details_shape_invalid`）。
     触发条件是 daemon 自身读写两端不一致，正常会话不会走到；但一旦走到，桌面壳原本
     看到的是"空白审批卡"，现在看到的是 HTTP 失败。
   - 正文夹具的值域修正（`filesystem`/`tier-2` 这类 daemon 永不发射的值从契约样例里消失）。

## 待决（必须人拍板）

1. **`COMMAND_CONTRACT_REVISION` / `SESSION_CONTRACT_REVISION` 的 v3.1 取值**。
   候选与代价：
   - **都不 bump**（保持 `-7` / `-2`）：v3.0 客户端可以直接连 v3.1 daemon（本轮改动确实
     兼容）。代价：`-2` 这把 revision 从此对应**两把不同的锁**（v3.0 与 v3.1 的
     `contract_hash` 不同），"握手代际"不再能唯一确定契约 —— ADR-0004 §已决 第 2 项
     （按 client revision 降级发射）将来就没法挂在这层信息上。
   - **只 bump session 面**（`ringing-session-3`，`-7` 不动）：本轮改动全在会话读模型的
     错误语义上，命令目录一个字节没变 ⇒ 语义最准。代价：老客户端 open 直接被 426 拒，
     必须等前端 pin 新契约才能发版；且 `qaqh-client/tests/v2_public_api.rs:36` 与
     `fixtures/*.json` 里的字面量要跟着机器重切。
   - **两个都 bump**（手册 §8 第 4 步的字面写法）：最保守也最贵 —— 把一次纯增量的
     契约补盲升级为命令目录换代，前端要重对新目录。
   倾向：**只 bump session 面**，但它要求 ADR-0006（契约发包 + hash 对齐）先有可用形态，
   否则"等前端 pin"没有落点。取值请仓库所有者定；本 ADR 不替它决定。
2. **切线时机**：现在就切 `LTS-v3.1`（只为 3 个产物），还是等 ADR-0004 的 `unknown` 那套
   与 ADR-0008 的 `schema/` 折入一起切（一次换锁吸收三批 refreeze，前端只 pin 一次）。
   倾向：后者；攒着的差额由本 ADR 记账，不会丢。
  （第二轮实测后追加：攒的那批里必须包含 IR 扩面，否则新线的 C# 面仍是裸奔 —— §背景 6。）
  > **已决（2026-11-10，仓库所有者）**：**`v3.1` 先不切。** 本 ADR 因此保持 `提议中`，
  > §攒着的清单继续累加；重提切线时必须先读本 ADR（含 §背景 6 的 `ir-diff` 数据）而不是
  > 从"main 那条红看着难受"出发。这条决定的效力等同于"暂缓 §待决 1/3"——
  > 在切线被重新批准之前，`LTS-v3.0` 仍是唯一的冻结线，`docs/api-surface/v3.0` 一字节都不动。
3. **错误语义变更要不要前端先行**：`qaqh-electron` 的 `pending_approvals` 消费方目前对
   500 的处理是什么、需不需要它先落一条确定文案，再让 v3.1 daemon 上线。
   这一条不在本仓可验证范围内（姊妹仓），必须由人确认。

## 攒着的 refreeze 清单（切线时一次吸收）

- [x] ADR-0004 前两条：`PermissionRequestDetails` 入导出物 + `.ok()` 静默降级改显式失败
      （本轮，`contract_hash` → `39256ad9…`）
- [ ] ADR-0004 §已决：出站 `unknown` 变体 + `variant_policy` 声明（TS / C# / schema 三面可见）
- [ ] ADR-0008：`schema/` 折入 `generate()`（同时 `GENERATOR_VERSION` 0.2.0 → 0.3.0，
      那一步会重写每个 C# 文件头的生成器注释 ⇒ 240 个文件的 `doc-only` 面）
- [ ] ADR-0005：N-1 配对测试（切线前置条件，ADR-0007 §决策 3 要求 refreeze 的 ADR 附其结论）
- [ ] **IR 扩面**（ADR-0008 第 1 步剩余部分）：`qaqh-config-api` → `qaqh-types` →
      `qaqh-ringing*` → `qaqh-runtime::service_methods`；覆盖率从 8/236 抬起来之前，
      "新线 C# 面无精度损失"这句话无法被任何工具验证（本轮实测）
- [ ] **Skills 子系统裁剪**（2026-11-12，`198e8ede`；exporter 对齐见同日修复提交）：
      `qaqh_types::{SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2}` 删除，
      `ToolEffectInfo` 失去 `skill_activated`/`skill_released` 变体（仅余 `subagent_spawned`）。
      导出物 **−6 文件**（3 类型 × cs/ts；check 实测 599→596，含 ADR-0004 的 +3），
      `contract_hash` 必变 ⇒ **wire 实变（类型/变体删除）**，切线时一次吸收。
      连带修复：ts-rs 对单变体内标枚举退化为「字面量首字段 plain object」，
      `cs_backend` 声明层将其归一为单变体 Union（否则 C# 投影报
      "string literal outside discriminator position"）。skills 服务端点
      （`skills.catalog`/`skills.list_tools`）与其参数类型不受影响。

## 被否选项

- **在 `main` 上就地重切 `docs/api-surface/v3.0`**：否。那棵树是冻结线的锁；main 上的副本
  一旦与 `LTS-v3.0` 上的副本不一致，"main ⊇ LTS"（规则 19）就退化成"main 假装包含 LTS"，
  后续 merge 会在金标准上互相覆盖。
- **现在提前生成一棵 `docs/api-surface/v3.1`**：否（仓库所有者 2026-11-10 明确选择）。
  revision 未 bump、线未切，这棵树不是任何分支的锁，却长得像锁 —— 下一个接手的人会把
  它的红/绿当成契约事实。
- **把 P0-b 推迟到切线再做**：否。盲区每多存在一天，`PermissionRequestDetails` 的字段
  变化就继续既不触 `contract_hash` 也不触守卫（ADR-0004 §发现 1）。
- **靠 `#[serde(other)]` / 兜底默认值把形状漂移继续"看起来能工作"**：否（宪法第 5 条，
  ADR-0004 §被否选项已记）。

## 后果

- `main` 的 `Golden contract byte compare` 步骤在切线之前一直是红的（report-only，
  job 仍绿）。这是**设计状态**，不是待修缺陷；手册 §1 判定法与本 ADR §背景 2 是唯一
  解释来源。看到它就去改金标准 = 违规（规则 16）。
- 切线时必须由机器写金标准（exporter + `--rebaseline`），并且三处指向一起改
  （`lts-api-guard.yml` 的 `branches:`、`check-lts-api-surface.py` 的 `--against` 默认值、
  `qaqh-ringing/src/v2/types.rs` 的 revision）—— 漏一处等于新线没有闸（手册 §8 第 4 步）。
- 金标准重切之后必须 `--pin-frozen`（ADR-0011）：`docs/api-surface/snapshots/frozen-locks.json`
  里没登记新线的锁身份 ⇒ 守卫直接红，不会静默放行。
- 若 §待决 1 选了 bump，`fixtures/*.json` 与 `constants.ts` 里的 revision 字面量会随机器
  重切而变化，`qaqh-client/tests/v2_public_api.rs:36` 的断言必须同步 —— 那是行为断言，
  改它要在同一提交里说明理由（手册 §1"夹具只能修编译性破损"的例外只在新线）。

## 落地清单

- [ ] 仓库所有者在 §待决 1/2/3 上给出取值（本 ADR 状态从 `提议中` 改 `已采纳`）
- [ ] 攒着的 refreeze 齐了 → 按手册 §8 切 `LTS-v3.1`（含新线分支保护，§7 命令换分支名）
- [ ] 切线后把本 ADR 标 `已实施`，并在 `docs/adr/README.md` 索引同步
