# ADR-0010 换锁：v3.1 线的待拍板取值与攒着的 refreeze 清单

状态: `已采纳 · 已实施`（2026-11-13 所有者指令切线并发版 v3.1.0；§待决 1/2/3 裁决见 §切线实施记录） · 起草日期 2026-11-10 · 起草人 QAQ agent

> 本 ADR **不定取值**，它把"已经攒在 `main` 上的 refreeze 差额"逐条记账，
> 并把手册 §8（换锁流程）里那几个必须由人定的空填成可勾的选择题。
> 引用本 ADR 时注意状态：**提议中 ≠ 已实施**。
> **2026-11-13 更新**：所有者先以「直接设计,现在就是 v3.1 窗口」（ADR-0019 决策人栏）
> 重开切线窗口，再以「升级到 v3.1.x 后提交全部代码，push，打 release tag，
> CI 打三平台二进制」指令执行发版。本 ADR 状态随之改为 **已采纳 · 已实施**；
> 下方 §待决 的原文按 ADR 不可变纪律保留，裁决以 §切线实施记录 为准。

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
   > **已决（2026-11-13，切线执行）**：取 `ringing-commands-9` / `ringing-session-2`。
   > 这不是新裁决，是追认账本里逐条已落的所有者决策：命令面 `-9` 由 ADR-0015 §已决 2
   > 拍板（Goal/truncate 同律）；会话面维持 `-2` 由本清单各 additive 条目
   > （流式预览、Handoff、exec「已决（2026-11-13，所有者）：本条不 bump」）一致定死。
   > 切线本身不再新 bump。原候选分析保留如下存档。
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
   > **已决（2026-11-13，仓库所有者）**：**切线窗口重开并执行。** 上一条「暂不切」
   > 被所有者新指令取代（ADR-0019 决策人栏「现在就是 v3.1 窗口」+ 本次发版指令）。
   > 攒着的 refreeze 批（含 IR 扩面以外的全部已落地条目）由 v3.1 金标准机器重切一次吸收；
   > 未实施条目（ADR-0004 unknown / ADR-0008 schema / ADR-0005 N-1 / IR 扩面）
   > 留在 main 快车继续演进，归下一条线（v3.2）的账本。
3. **错误语义变更要不要前端先行**：`qaqh-electron` 的 `pending_approvals` 消费方目前对
   500 的处理是什么、需不需要它先落一条确定文案，再让 v3.1 daemon 上线。
   这一条不在本仓可验证范围内（姊妹仓），必须由人确认。
   > **已决（2026-11-13，切线执行）**：不阻塞后端发版。所有者发版指令未附前端先行条件；
   > 前端连带（pin v3.1.0 契约 zip、500 文案、Team inbox union 渲染、设置页删子代理段）
   > 记入 `docs/lts-v3.1-maintenance.md` §7.1 前端连带段，由前端仓同批发布消化。
   > 旧客户端对 v3.1 daemon 的 `-7`/`-8` 命令代际一律 426/428 拒连是设计语义，不做半兼容。

## 攒着的 refreeze 清单（切线时一次吸收）

- [x] **子代理配置面退役**（2026-11-13，ADR-0018，代码已落 main；**v3.1 机器重切吸收**）：
      `ConfigDto.subagent` / `ConfigPatch.subagent` 字段删除 +
      `SubagentDto` / `SubagentPatch` 两型删除（ts + cs 各 2 文件，
      index/coverage/manifest 随机器重切）⇒ **wire 实变（字段/类型删除）**，
      切线时一次吸收。
      - 命令面零新增；`COMMAND_CONTRACT_REVISION` 不涉及。
        `SESSION_CONTRACT_REVISION` 不 bump（读模型字段删除的代价归 §待决 1
        统一裁决，与 Goal 段新增同律：ConfigDto 无向前兼容，前后端共进退）。
      - 同 ADR 的内核行为变化（SubagentView 终态说真话 / gate 掐流=失败 /
        120s 超时清扫退役 / 内部 ToolEffect 超时字段删除）**导出物零变化**，
        不计入本条。
      - 前端连带：设置页删子代理段；Team 页补 failed/cancelled 文案；
        前后端同批发布（旧客户端解析新 ConfigDto 缺字段失败）。
- [x] **Goal 模式**（2026-11-13，ADR-0015，代码已落 main 工作区；**v3.1 机器重切吸收**）：新增命令
      `goal.set` / `goal.get` / `goal.clear`、事件 `SessionEvent::GoalUpdated`
      （`goal: Option<GoalView>`，None=清除）、DTO `GoalView` / `GoalStatus`、
      `SessionState.goal` 字段、拒绝码 `goal_exists` / `no_goal`、配置段
      `ConfigDto.goals` / `ConfigPatch.goals`（`GoalsDto` / `GoalsPatch`）⇒
      **wire 实变（命令/事件/DTO 新增 + 读模型字段新增）**，切线时一次吸收。
      - `COMMAND_CONTRACT_REVISION` 已 bump `ringing-commands-8` → `-9`
        （ADR-0015 §已决 2，所有者拍板，与 truncate 同律）；5 个常量产物的值差异
        一并记账。`SESSION_CONTRACT_REVISION` 仍 `-2`（事件/读模型纯增量，
        沿用本 ADR 对 additive 的口径）。
      - 会话库 schema 8→9：`session_goals` 表纯增量迁移，无存量归一。
      - **切片F 追加**：`session.create` 增可选 `handoff_from`（additive）；
        `sessions` 表增 `handoff_from_session_id` / `handoff_sealed_at_ms`
        两列（同入 schema 9）。goal 沿 handoff 工作链解析,见 ADR-0015 §切片F。
      - `ConfigDto` 无向前兼容（crate 纪律）：旧客户端缺 `goals` 字段解析失败，
        前后端共进退——与既有读模型口径一致，不另开兼容层。

- [x] **Handoff 内核 fork**（2026-11-13，ADR-0016，切片 1–3 代码已落工作区；**v3.1 机器重切吸收**）：
      新增事件 `SessionEvent::HandoffSealed`（`sealed_session_id` / `chain_root` /
      `successor_session_id` / `turn_id` / `work_summary` / `at_ms`）、读模型
      `SessionMeta.handoff_from_session_id`（additive，随 `session.list` /
      `session.meta` 导出，G2 键表同步）⇒ **wire 实变（事件变体 + 读模型字段）**，
      切线时一次吸收。
      - 命令面零新增（fork 是内核内部动作）；`SESSION_CONTRACT_REVISION` 仍 `-2`
        （事件/读模型纯增量，沿本清单对 additive 的口径）；`contract_hash` 已随
        export 重打。
      - 产物 diff：SessionEvent / SessionEventFrame / SessionMeta /
        SessionListEntry（ts + cs）、fixtures（session-list / session-meta）、
        manifest（hash 与 coverage）。

- [x] **exec 完整输出与只读终端**（2026-11-13，ADR-0017，已采纳·部分实施；**v3.1 机器重切吸收**）：新增 service 方法
      `exec_session.output`（offset 区间读）与 DTO `ExecSessionOutputParams` /
      `ExecSessionOutputResult`（result 为单 `text` 形状；ADR-0017 决策 2 修订：原
      `chunks/source` 形状被否——日志只存解码拼接文本，无 source 维度）⇒
      **wire 实变（service 方法 + 产物新增）**，切线时一次吸收。
      - 路由零新增（走既有 `POST /ringing/v2/service/{method}`，两张路由表不动，规则 16）。
      - 事件词表零变更（`ExecSessionOutputDelta` 原样）；模型侧零改动；不引入 WebSocket。
      - 终态 transcript 去 1MiB 封顶属数据/行为变化，无 wire 形状变更，不计入本条。
      - **已决（2026-11-13，所有者）：本条不 bump**（基线版本号不动，`SESSION_CONTRACT_REVISION`
        维持 `-2`；命令面零新增，`COMMAND_CONTRACT_REVISION` 不涉及）。
      - 实测（2026-11-13，`qaqh-contract-export` 本机 `export`）：导出物 **608 → 614**
        （基线 = 当日 main 工作区 golden），only-in-first 恰为本条 6 文件：`types/qaqh/ExecSessionOutputParams.ts` /
        `ExecSessionOutputResult.ts`、`cs/ExecSessionOutputParams.cs` /
        `ExecSessionOutputResult.cs`、`fixtures/service-exec_session-output-params.json` /
        `service-exec_session-output-result.json`；only-in-frozen 为空。
        `contract_hash`：工作区 golden `1ae84972a3a0ccd1759b575c63eadd4330bc26881bc9e14d9cf391218e5111f0`
        → 本条导出树 `b96eb79f6ab15226445540d485d8dfcd93a9fbdd20679598c2d2421794e9c426`
        （本机实测；对照 HEAD 冻结锁：601 文件 / `1a0db0e0a22bdca154c4eeaa18b1fa7461732dbd7727fad45aa038f8488a7fb0`）。
        本条未触碰 `docs/api-surface/v3.0/`；`check --against` 在 main 上红为设计状态
        （§背景 2）。
      - **并发留痕（宪规 19）**：2026-11-13 工作区中 `docs/api-surface/v3.0/` 存在
        **非本条所为的在途改写**（57 处：Skill* 三型删除、Goal* / PermissionRequestDetails /
        Widget* 新增、manifest/index/coverage/constants 变更，601 → 608 文件）——与
        ADR-0010 账本其他在途条目同源，不是重切禁令的私下豁免；本条差额以该工作区状态
        为基线实测，切线吸收时以届时统一重切的 golden 为准。
      - 前端连带：ServiceMap + native addon `serviceMethods()` 白名单加方法（packaging
        测试强制一致）；exec 输出 ref 懒水合 + fetch-on-open + 缺口补读。
- [x] **Team inbox InterAgentActor 寻址统一**（2026-11-13，ADR-0019，读侧代码已落工作区；**v3.1 机器重切吸收**，死变体退役随切线实施）：
      新增导出类型 `InterAgentActor`（`path(AgentPath) | session(SessionId)`）+
      `TeamInboxSummary.author/recipient` 字段类型 `AgentPath → InterAgentActor`
      ⇒ **wire 实变（类型新增 + 读模型字段类型变）**，切线时一次吸收；金标准不动，
      机器重切（规则 16）。同轮记录：`FactPayload::InterAgentCommunication` 死变体
      退役（类型/变体删除，wire 实变）已入 ADR-0019 §决策 4，实施随切线清单执行，
      避免本轮牵连 fact validation/slots/fixture 扩散。前端连带：Team 页 inbox
      行渲染改为判别 union；跨根条目现可点击 → `send_message` 以 session id 回信。
- [x] ADR-0004 前两条：`PermissionRequestDetails` 入导出物 + `.ok()` 静默降级改显式失败
      （本轮，`contract_hash` → `39256ad9…`）
- [ ] ADR-0004 §已决：出站 `unknown` 变体 + `variant_policy` 声明（TS / C# / schema 三面可见）
- [ ] ADR-0008：`schema/` 折入 `generate()`（同时 `GENERATOR_VERSION` 0.2.0 → 0.3.0，
      那一步会重写每个 C# 文件头的生成器注释 ⇒ 240 个文件的 `doc-only` 面）
- [ ] ADR-0005：N-1 配对测试（切线前置条件，ADR-0007 §决策 3 要求 refreeze 的 ADR 附其结论）
- [ ] **IR 扩面**（ADR-0008 第 1 步剩余部分）：`qaqh-config-api` → `qaqh-types` →
      `qaqh-ringing*` → `qaqh-runtime::service_methods`；覆盖率从 8/236 抬起来之前，
      "新线 C# 面无精度损失"这句话无法被任何工具验证（本轮实测）
- [x] **Skills 子系统裁剪**（2026-11-12，`198e8ede`；exporter 对齐见同日修复提交；**v3.1 机器重切吸收**）：
      `qaqh_types::{SkillSessionEntry, SkillSessionEntryState, SkillSessionStateV2}` 删除，
      `ToolEffectInfo` 失去 `skill_activated`/`skill_released` 变体（仅余 `subagent_spawned`）。
      导出物 **−6 文件**（3 类型 × cs/ts；check 实测 599→596，含 ADR-0004 的 +3），
      `contract_hash` 必变 ⇒ **wire 实变（类型/变体删除）**，切线时一次吸收。
      连带修复：ts-rs 对单变体内标枚举退化为「字面量首字段 plain object」，
      `cs_backend` 声明层将其归一为单变体 Union（否则 C# 投影报
      "string literal outside discriminator position"）。skills 服务端点
      （`skills.catalog`/`skills.list_tools`）与其参数类型不受影响。
      **数据层补齐（同日）**：类型裁剪后旧库 `tool_calls.effects_json` 存量仍带
      legacy kind，读侧 fail-closed（ADR-0004 §决策 4）把含 legacy 行的历史会话
      永久毒化为 bootstrap/approvals 500（`DbError::Corrupt` → `state_unavailable`）。
      落 schema **7→8 一次性迁移**：精确剔除 `kind` 为 `skill_activated`/`skill_released`
      的数组元素，清空落 NULL（与写侧形态对齐）；非 JSON 数组的行原样不动——
      那不是裁剪的债，仍归读侧如实报。反证测试 `#[ignore]` 于生产库副本跑红→绿。
- [x] **流式参数预览字段**（2026-11-12，A+B 工具卡片时机；**v3.1 机器重切吸收**）：
      `SessionEvent::ToolArgumentsDelta` 新增 `target_files: Vec<String>` 与
      `args_complete: bool`，两者都是 `#[serde(default, skip_serializing_if = …)]`
      ——**additive**：旧客户端读不到就忽略，新客户端把缺席当默认（与同变体里
      `lines_added: Option<_>` 的「缺席 ≠ 零」口径同律）。
      - `target_files` 只有 `apply_patch` 会填：它的参数里没有路径字段，文件名只存在于
        Codex 补丁文法里，而该文法的唯一事实源是 `StreamingPatchParser`（`stats().files`）。
        其余工具的路径就是参数键本身，客户端按 schema 直读 `args`，内核不再发明 JSON 键名。
      - `args_complete` = 参数文本闭合可 parse（JSON 面）或已见 `*** End Patch`（原生面）。
        只说明「这一帧的 `text` 就是最终参数」，不授权执行，也不代替 `ToolDeclared` 的权威 `args`。
      发射侧连带：`PROGRESS_INTERVAL` 的文档早就承诺 "Final values are always published
      regardless of this interval"，但工具参数通道此前只在片段到达时按 40ms 窗发，
      尾帧被吞（`provider_job::stream_once` 全出口现已补齐），且闭合那一帧改为抢发。
      导出物影响：`types/qaqh/SessionEvent.ts` 与 `cs/SessionEvent.cs` 字节变 ⇒
      **wire 实变（产物字段新增）**，金标准留给切线时由机器重切（规则 16），main 上
      `docs/api-surface/v3.0` 一个字节不动。
      口径选择：**不 bump** `SESSION_CONTRACT_REVISION`——沿用本 ADR §背景 4 对纯增量的
      判断（旧客户端只是看不见新键）；"`ringing-session-2` 从此对应两把锁"的代价仍归
      §待决 1 统一裁决，不在本条偷跑 bump。
      前端连带：`webui` reducer 把两字段落到 `step.targetFiles/argsComplete`，
      `toolPaths` 优先采用内核结论，并**删除**客户端自建的补丁头正则——同一份文法
      不在两端各养一遍（宪法 4/6）。
- [x] **消息撤回（截断）**（2026-11-12，`conversation.truncate`；**v3.1 机器重切吸收**）：新增命令
      `conversation.truncate { turn_id }` 与事件 `SessionEvent::TimelineTruncated {
      from_turn_id, removed_turn_ids }` ⇒ **wire 实变（命令枚举新增变体 + 事件新增变体）**，
      触及 `RingingCommand.ts/.cs`、`SessionEvent.ts/.cs`。语义与取舍：
      - **档位 = 尾部截断（A+B）**，语义是「从该回合（**含自身**）起的整段历史删除」，
        不是「撤回单条中间消息」。中间撤回（C）会静默腐化历史：`assemble_context` 的
        `ToolResult` 分支取不到 tool 时走 `unwrap_or(false)`，删 `tool_calls` 留
        `ToolResult` 不会 400，而是把那次调用**改写成历史上失败**。要做 C，前置是给
        tool_use/tool_result 配对加硬校验（孤儿当场报错），那是独立一条 ADR，不在本轮。
      - **硬删除、不落备份**（用户口径「丢失即可」）：`store::truncate_from_turn` 在同一
        事务里删 `turns`/`blocks`/`tool_calls`/`interactions`/`exec_sessions`。块按会话
        `ord` 截断而非逐条匹配 `turn_id`，否则回合归属为 NULL 的压缩摘要会留下
        「历史已删、摘要还在」的半截事实。
      - **只回历史，不回副作用**：文件改动、执行会话进程、子代理、已通过的审批一律不动；
        `journal` 原样保留（要回滚文件另按它走，本轮不做）。
      - **不允许冲突撤回**：存在 `status='active'` 回合时当场拒（`DbError::TruncateConflict`），
        既不写也不发事件。判据落在写点（owner 泵串行化了它与 `start_turn`），调用方不
        重复设第二道判据。重启遗留的 active 行由 restart recovery 统一置 `interrupted`，
        不会永久锁死撤回口。
      - **事件流自身仍 append-only**：撤回是**追加**一条截断事实，不回收已发的帧——
        `SessionEventFrame.seq`  epoch 内 0 基单调、bootstrap `state.seq` 是水位线、客户端
        靠「跳过 `< state.seq`」自愈（`events.rs` 头注明写不得单方改动）。断线重连的客户端
        无需处理该帧：快照里本就没有被删的回合。
      - 路由零新增：命令走既有 `POST /ringing/v2/commands`，两张路由表（规则 16）不动。
      - **拒绝码进命令面词汇表**（同日 B 决定）：`qaqh_ringing::CommandRejection`
        （`turn_active` / `turn_not_found`）+ `QaqhError::Rejected { code, message }`
        原型；daemon 侧 `Route::Engine` 收口把它渲染成 409 / 404 + 该码，其余仍走
        `dispatch_failed`。落地的连带修复：`RuntimeService::send_ringing_command`
        原本 `-> Result<(), String>`（在边界上 `to_string` 掉变体，正是 daemon
        `commands.rs` §"Conflict 不得伪装成 502" 那条教训的同款缺陷），已改回
        `QaqhResult<()>`。码字符串由 `qaqh-ringing` 单测钉住
        （`truncate_rejection_codes_are_stable`）。先例：`46e6dc76 [contract-refreeze]`
        「approvals 错误码进契约」——错误码扩面本就按 refreeze 记账。
        前端连带：`CommandRejectedError`（`electron.ts`）把 `ack.code` 提成可分型字段，
        `message` 拼装逐字未变，故现有只读 `error.message` 的 catch 行为不变；
        `TurnView` 按码分三路文案。
      - **已 bump `COMMAND_CONTRACT_REVISION` → `ringing-commands-8`**（2026-11-12，
        仓库所有者口径：既然会话库 `SCHEMA_VERSION` 已到 8，命令面这代也升 8）。
        按该常量自己的头注「字段和语义变更需显式升级」，新增命令 + 新增拒绝码就是
        升级触发条件。**代价如实登记**：
        - open 握手对仍报 `-7` 的客户端返回 428 `unsupported_contract_revision`
          （`axum_impl/v2.rs:56`）；`contract_revision` 为 `-7` 的信封在
          `RingingV2CommandEnvelope::validate()`（`types.rs:141`）当场拒。即**未重编的
          旧客户端一律拒**，不做半兼容——这与 spec §11 的既有闸门语义一致。
        - 与 `ADR-0001` §「`ringing-commands-7` 在整个 LTS 周期保持原值」和
          `docs/lts-v3.0-maintenance.md` §「保持原值不动」**正面冲突**。本条是后发的
          决策记录，两处原文按「ADR 不可变」不改写；维护手册那句在 v3.0 线上仍成立，
          冲突由 §待决 2「v3.1 先不切」吸收：main 上带着 `-8`，`LTS-v3.0` 若要合入
          这个提交必须先决定要不要破它自己的锁（超集不变量在此处不再免费）。
        - 冻结金标准有 **5 个产物的值**由本常量机器生成（`constants.ts`、
          `cs/ContractConstants.cs`、`fixtures/{open-request,open-response,command-attach}.json`），
          本轮一个字节未手改；`check --against docs/api-surface/v3.0` 的产物差异表
          因此在原有「文件集 596 vs 599」之外新增这 5 项**值差异**，留给切线时
          exporter + `--rebaseline` 机器重切（规则 16）。
        - `SESSION_CONTRACT_REVISION` 仍 `ringing-session-2`（本轮只升命令目录代际；
          事件面那两把锁的代价继续挂 §待决 1）。
      - 前端连带：`reducer.removeTurns` 按后端给出的 `removed_turn_ids` 整枝移除
        `turns`/`slots`/`waits`/`interactionIds`/`execSessions`，`totalTurns` 用后端权威条数
        递减（未加载的回合也在删除之列，本地遍历数不出来）；撤回按钮只在非运行中的回合
        出现，本地**不做乐观删除**——删除只有一个来源，即事件。

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

- [x] 仓库所有者在 §待决 1/2/3 上给出取值（2026-11-13 裁决见各 §待决 追加块与 §切线实施记录）
- [x] 攒着的 refreeze 齐了 → 按手册 §8 切 `LTS-v3.1`（含新线分支保护，§7 命令换分支名）
- [x] 切线后把本 ADR 标 `已实施`，并在 `docs/adr/README.md` 索引同步

## 切线实施记录（2026-11-13）

**触发**：所有者指令「升级到 v3.1.x 后提交全部代码，push，打 release tag，
ci/action 打 Windows/Linux/macOS 二进制」。窗口重开的先声是 ADR-0019 决策人栏
「直接设计,现在就是 v3.1 窗口」（2026-11-13）。

**执行**（手册 §8 逐步）：

1. main 落 `[contract-refreeze]` 提交：ADR-0019 读侧 + 死变体退役（决策 4，
   连带孤儿 `InterAgentContent` 与 `MAILBOX_TEAM` 常量一并删除）+ 版本戳
   `3.1.20261113-g94e719d`（五处同步）+ release.yml 三平台矩阵 + 本 ADR 裁决。
2. `git switch -c LTS-v3.1 main`；机器重切金标准：
   `QAQH_SOURCE_REVISION=<HEAD> QAQH_SOURCE_DIRTY=0 cargo run -p qaqh-contract-export -- export --out docs/api-surface/v3.1 --release`
   → `--rebaseline`（routes/facade 快照）→ `--pin-frozen`（锁身份），同一提交。
3. 改指向（比手册 §8 第 4 步多两处——手册漏记了分类闸与留痕闸的指向，本次补齐）：
   - `lts-api-guard.yml` branches → `LTS-v3.1`
   - `check-lts-api-surface.py` 默认 `--against` → `docs/api-surface/v3.1`（失败文案同改）
   - `surface-policy.yml` / `sdk-contract.yml` branches → `[main, LTS-v3.1]`；
     surface-policy 的 `continue-on-error` target 与 `check --against` → v3.1
   - `check-surface-change-policy.py` `FROZEN_BRANCHES` += `LTS-v3.1`（v3.0 保留）
   - `check-frozen-ledger.py` `LEDGER_FILE` → `docs/lts-v3.1-maintenance.md`（新线新账本）
   - revision 常量**不动**：`ringing-commands-9` / `ringing-session-2` 已是 main 现值。
4. 新线分支保护（§7.5 基线换分支名：PR + contract/freeze/change-policy + enforce_admins）。
5. ff-merge 回 main（main ⊇ LTS-v3.1），push 两分支，切 `v3.1.0` tag → release.yml
   三平台构建 + 契约 zip + GitHub Release。

**实测**（本机，切线提交）：

- 导出物文件数与 `contract_hash`：见 `docs/api-surface/v3.1/manifest.json` 与
  `docs/api-surface/snapshots/frozen-locks.json`（机器写，此处不复述防漂移）。
- 反证（宪规 18）：`check-surface-change-policy.py --base <切线前 main> --head <切线提交> --target LTS-v3.1`
  判红（refreeze 禁入冻结线）；`--target main` 判绿。切线首推本身走新分支
  before=0 跳过语义（与 v3.0 金标准引入 `e3b2b618` 同例），不是闸门的豁免。
- `check-frozen-ledger.py --self-test` 绿；`check-lts-api-surface.py`（v3.1 默认）绿。

**事实留痕**：

- 切线时 `git ls-remote --heads origin` 只有 `main`——`LTS-v3.0` 分支在远端已不存在
  （手册 §7.5 的保护基线是历史快照）。v3.0 的锁以金标准树 + frozen-locks 登记 +
  `v3.0.0` tag 形态保留，`compare_frozen_locks` 继续硬比它的 tree OID；
  工作流触发面不再列 LTS-v3.0（无分支可触发，留着是兼容残骸，宪法 4）。
- §被否选项「现在提前生成一棵 v3.1」的否决前提是"线未切"；本次切线已获所有者批准，
  该前提消失，v3.1 金标准是名正言顺的新线锁，不是"长得像锁的树"。
- 未实施条目（ADR-0004 unknown / ADR-0008 schema / ADR-0005 N-1 配对 / IR 扩面）
  不阻塞本次切线：它们没有落地代码，无差额可吸收；继续留在 main 演进，
  下一条线（v3.2）切线时再吸收。ADR-0005 作为"切线前置"的原要求由所有者发版指令豁免，
  豁免留痕于此，防重提时误以为它已实施。
- **v3.0 锁污染恢复（切线守卫当场揪出）**：`4661a62a` 曾在 main 上就地改写
  `docs/api-surface/v3.0/`（56 文件 +636/−270：Skill* 三型删、Goal*/Widget*/
  PermissionRequestDetails 增——与 ADR-0017 §并发留痕记录的在途改写同源），
  树 `f2f8561e…` 偏离登记身份 `e0fc731c…`（`4e0f7160` 写 pin）。切线时全量守卫
  按 ADR-0011 设计判红。处置：v3.1 内容已有自己的合法 golden，故把 v3.0 树
  **恢复到 pin 提交 `4e0f7160` 的树**（`git restore --source`，逆操作 +270/−636，
  随切线提交入库），**不** re-pin 追认污染——re-pin 会把 v3.1 代际的 wire 内容
  冻成"v3.0 锁"，那才是偷换锁。恢复后 `HEAD:docs/api-surface/v3.0 == e0fc731c…`，
  锁身份闸绿。这是恢复锁身份，不是就地重切：冻结树回到登记字节，漂移内容
  迁居新线 golden。污染期 main CI 的 `--skip-export` 硬闸步骤应当是红的
  （job 绿不代表步骤绿，宪规 18），本次切线一并治愈。
- **surface-policy 的 mutil-ai 旧 pin（切线硬闸揪出，随 PR 修复）**：四个工作流里
  唯独 `surface-policy.yml` 还 pin 旧 `4387f74d`（缺 `ChatRequest.prompt_cache`，
  `qaqh-gate` 编不过）。main 上被 `continue-on-error` 掩盖（job 绿、步骤红，
  宪规 18 的活样本），切线后 LTS 硬闸当场红。修复：与其余三个 workflow 同 pin
  `b37ca25f`（手册 §4 钉子），走 PR 进 LTS-v3.1 再 ff 回 main。
