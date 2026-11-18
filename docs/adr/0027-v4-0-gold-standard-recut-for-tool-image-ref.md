# ADR-0027 在 main 上就地机器重切 v4.0 金标准，以追认 `ToolImageRef`

状态: **已采纳 · 已实施**（2026-11-17 所有者裁定，见 §决策 与 §裁定原话） · 起草日期 2026-11-17 · 起草人 QAQ agent

> **状态更新 2026-11-19（ADR-0031）**：规则 15 字面「refreeze ⇒ 开新 LTS 线并 bump
> revision」已被 ADR-0031 对 main 线（v4+）松绑（所有者裁定：tag ≠ LTS，非 LTS 发版合法）。
> 本文 §已知不严格 重新定性：三戳未 bump = **正常形态**（服务方法与 additive 增量不触碰
> 三戳语义对象，ADR-0031 决策 4，同时销掉 ADR-0028 更正记录的「服务面 revision 位」张力）；
> 补丁位吸收 refreeze 维持「登记过的偏离」定性，今后默认走 minor 版本位（ADR-0031 决策 3）。
> 原文按 ADR 不可变纪律保留。

> 作用域声明：本 ADR 只管 **`main` 上的 `docs/api-surface/v4.0/`**。
> ADR-0010 §5 那条「不得为了让它变绿而重切金标准」约束的是 **`LTS-v3.0` 分支持有的
> `docs/api-surface/v3.0/` 锁**，两者不是同一个所有权对象，本 ADR 不构成对该条的推翻。

## 背景（全部为实测事实）

1. `cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.0` 在
   `cebbf96d` 上**实测 exit 1**：
   `artifact file set differs: 606 vs 604 files; only-in-first=[cs/ToolImageRef.cs,
   types/qaqh/ToolImageRef.ts], only-in-second=[]`。
2. 漂移源不是本次修复批次：`ToolImageRef` 由 `08f26e1b`（"工作区全量入库 WIP"）引入，
   定义在 `crates/qaqh-ringing-contract/src/state.rs:241-250`，带
   `#[cfg_attr(feature = "ts", derive(TS), ts(export))]`，并给导出枚举
   `TimelineEntry::ToolResult` 加了字段 `images: Vec<ToolImageRef>`
   （`:225-230`，`serde(default, skip_serializing_if = "Vec::is_empty")` ⇒ additive）。
3. **用户空间早已消费它**：`D:\Projects\qaqh-electron\webui\src\api\qaqh\ToolImageRef.ts`
   已存在，且同目录 `TimelineEntry.ts:13` 已 `import type { ToolImageRef }`。
   落后的只有后端金标准目录 —— 它从没为重切过一次。
4. 实质漂移面经 `git diff --numstat` 收敛为 **6 改 + 2 新增**：
   `ToolImageRef.{ts,cs}`（新增产物）、`TimelineEntry.{ts,cs}`（`images` 字段）、
   `index.ts`（一条 export）、`manifest.json`（清单/哈希 + `backend_version`）、
   `fixtures/{pair-result,service-daemon-version-result}.json`（版本戳 `4.0.0→4.0.1`）。
   其余 18 个 `git status` 显示 ` M` 的金标准文件 **内容 0 变化**，纯本机
   `core.autocrlf=true` 的 EOL 记账噪声（exporter 写 LF，检出为 CRLF）。
5. 闸门现状（决定"谁能判分类"）：`scripts/check-surface-change-policy.py` 与
   `.github/workflows/surface-policy.yml` 已被 `da33cbae`（"4.0 from-scratch: strip
   v3.x LTS gates"）一并剥离，`.github/workflows/` 现仅剩 `release.yml`（触发条件
   `push tags v*` 或 `workflow_dispatch`，且 **不跑测试**）。⇒ 唯一还在跑的机器判据
   是 exporter 自己的 `check`。ADR-0011 的"锁身份登记 + tree OID 硬比"同批失效。

## 决策

在 `main` 上用**机器**（`cargo run -p qaqh-contract-export -- export --out
docs/api-surface/v4.0`，规则 16：金标准只能由 exporter 写，一个字节都不手改）重切
v4.0 金标准以追认现实，随后发 `v4.0.1`。

### 裁定原话（所有者，2026-11-17）

> 「用户空间不可变只在 LTS 仓库，当前无 LTS 发版计划。」

即：规则 21 的用户空间不可变约束绑的是 LTS 产物线；`main` 上的 v4.0 目录不因目录名
而获得冻结身份。故"追认已发布现实"优先于"保住 v4.0 补丁位的字节不变"。

## 已知不严格之处（写死在此，防止被当作先例）

- 按规则 15 的字面，**产物集合增删属 `[contract-refreeze]`，应当 bump revision 并开
  新 LTS 线**。本次**没有** bump：重切后的 `constants.ts` 仍是
  `COMMAND_CONTRACT_REVISION = "ringing-commands-10"`、
  `SESSION_CONTRACT_REVISION = "ringing-session-3"`，`manifest.wire_version` 仍是 `2`。
  这三个戳是 Rust 侧常量、由 `export` 原样投影，不重切就还是旧号 —— 也就是说
  v4.0.1 的 SDK 契约 zip 携带一个 additive wire 增量却挂着未 bump 的 revision。
- 之所以判定风险可接受：该增量是 `#[serde(default)]` + `skip_serializing_if` 的
  additive 字段与新类型，旧客户端读不到 `images` 即空，不构成破坏；且 `images`
  早已被前端消费，**不发版才是与现实脱节的那一方**。
- 一旦真要开下一条 LTS 线：`--pin-frozen` 与重切金标准必须落在同一个提交
  （ADR-0010 手册 §8 第 3 步），且**必须**在同一步 bump 上面三个戳，否则本 ADR
  这条欠账会被复制一遍。

## 被否方案（留痕防重提）

1. **摘掉 `ToolImageRef` 的 `ts(export)`（或回退那部分导出）以保持金标准字节不动**：
   否。前端 `webui/src/api/qaqh/` 已在消费该类型，摘导出面等于破坏已发布的用户空间，
   且要改的是契约 crate 的导出属性，比追认更伤。
2. **严格按规则 15 字面开 `v4.1.0` 线并 bump revision，`v4.0.1` 只留给纯补丁**：
   否（所有者裁定）。当前无 LTS 发版计划；且现行树根本产不出符合 v4.0 冻结面的二进制
   （`ToolImageRef` 已在 main 的源码里），走这条只能先把已消费的能力回退掉。
3. **不重切、不发版，tag 无限期挂起**：否。`main` 在 `08f26e1b` 之后是**编不过**的状态
   （`session_loop.rs:654` 缺 `mut` ⇒ 4×E0594），本次已随 `cebbf96d` 修好，急需一个可
   装的后端二进制；把发版卡在一条无人认领的金标准欠账上不成立。

## 后果

- `check --against docs/api-surface/v4.0` 由红转绿（**重切后实测 exit 0**）。
- `v4.0.1` 触发 `release.yml` 产出三平台 daemon + `qaqh-sdk-contract-v4.0.1.zip`；
  该 zip 首次与前端已消费的 `ToolImageRef` 对齐。
- 规则 15 的分类标签由提交者声明改为**以本 ADR 为准**：引用本 ADR 时必须看状态字段，
  且不得把"追认 additive 增量"推广成"refreeze 不需要 bump revision"。
