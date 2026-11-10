# ADR-0011 冻结线的锁必须有登记身份：main 上"改金标准让它变绿"这条路封死

状态: `已采纳`（agent 落地于 `main`，待仓库所有者复核 —— CODEOWNERS 已覆盖守卫脚本） · 决策日期 2026-11-10

## 背景（已核对事实）

1. **分类闸看不见"改了契约但没碰金标准"的提交**。本轮 `contract-refreeze`
   （`PermissionRequestDetails` 入导出物，commit `5880a867`）刻意**不动**
   `docs/api-surface/v3.0`，于是 `scripts/check-surface-change-policy.py` 对它输出
   `未触碰冻结金标准，规则不适用`（实测 exit 0）。它的判据一直是"改动落点是否落在
   金标准目录里"（ADR-0007 §决策 2），不是"契约是否变了"。
2. main 上唯一能看见契约漂移的是 `surface-policy.yml:78-80` 的金标准逐字节比对，
   而它 `continue-on-error: ${{ target != 'LTS-v3.0' }}` —— **report-only**
   （ADR-0007 §决策 4 故意如此：main 允许演进）。
3. 1+2 合起来是一条 fail-open：在 `main` 上把
   `cargo run -p qaqh-contract-export -- export --out docs/api-surface/v3.0` 跑一遍
   （或直接手改一个产物），那条红就"消失"了，job 全绿，而规则 16「金标准只能机器写、
   不得手改」在 main 上没有任何硬闸守着。`export()` 的目录所有权检查只比
   `generator_version`（本轮实测保持 `0.2.0` ⇒ 不会拒绝就地重切）。
4. 这条路不是理论风险：`ac54e508` + `4c3a26be` 就是**在 LTS 上就地重切金标准**
   （事后被判 `doc-only-refresh`，ADR-0009）。"重切一下让它绿"是这个仓库真实发生过的动作。

## 决策

1. **每条冻结线的金标准树身份必须登记**：`docs/api-surface/snapshots/frozen-locks.json`，
   唯一写法是 `python scripts/check-lts-api-surface.py --pin-frozen`（机器写，同规则 16）。
   每条记录：`git_tree`（树 OID，锁的身份）、`contract_hash`、`manifest_files`、
   `generator_version`（后三者是人读附注，不参与比对）。
2. **守卫每次运行都比对 `HEAD:<金标准目录>` 的树 OID 与登记值**，且**在 `--skip-export`
   下也跑**（`compare_frozen_locks()`）。`surface-policy.yml:74-75` 在 main 与 LTS 上
   都硬跑这个脚本 ⇒ 这条不享受 report-only，是硬闸。
3. **锁文件放在被钉的树之外**（否则改写锁会改变被钉的 OID，自指），并且放在
   `snapshots/` 前缀下，以复用分类闸已有的判定「只动 `snapshots/*` ⇒
   `guard-tightening`」—— 登记一把锁是收紧守卫，不是换锁，**不新增第四类**。
4. **切线时 `export` + `--rebaseline` + `--pin-frozen` 落在同一个提交**：
   `--pin-frozen` 用临时 index 从**工作区**算出将来会被提交的树 OID
   （`tree_oid_from_worktree()`），因此不需要"先提交再补锁"这种中间红。
   已实测：工作区算出的 OID 与 `git rev-parse HEAD:docs/api-surface/v3.0` 一致
   （`e0fc731c38973a114336681d82567c9fa3ab9ed9`）。
5. **新线未被登记即判红**（`golden_rel not in entries` ⇒ problem），不做静默放行。

## 被否选项

- **把 main 上的金标准比对从 report-only 改硬拦**：否。那等于取消 main 的演进权
  （ADR-0001 §决策、ADR-0007 §决策 4）。要拦的是"改锁"，不是"锁与代码不一致"。
- **在 main 上提前生成一棵 `docs/api-surface/v3.1`**：否。仓库所有者 2026-11-10 已明确
  选择保持 v3.0 原样；理由与后果记在 ADR-0010 §被否选项。
- **用 `contract_hash` 当锁身份**：否。Python 得重算一遍 exporter 的哈希定义
  （逐文件 sha256 → 对 `[{path,sha256}]` 的 JSON 再 sha256）—— 那是**第二个语义定义**，
  ADR-0009 刚为"两套定义不一致"付过学费。`git tree OID` 本身就是内容身份，天然平台无关
  （`.gitattributes` 已把金标准钉成 `eol=lf`），零重复实现。
- **靠 bump `GENERATOR_VERSION` 让 `export` 拒绝覆盖**：否（本轮试过又撤）。它会把
  240 个 C# 文件头的生成器注释一起重写（实测），即用一次大规模产物变动换一条保护；
  且 `0.2.0 → 0.3.0` 是 ADR-0008 折入 `schema/` 那一步的预留 bump，不该被抢走。
- **"锁文件和树一起改就放行"**：不否决，但把它变成留痕动作 —— 一起改意味着同时动了
  `docs/api-surface/**`（CODEOWNERS 强制 owner 评审）与 `snapshots/*`
  （分类闸要求显式打 `guard-tightening` 标签）。偷换锁从此至少留两处名。

## 后果

- **覆盖边界要说清**：本闸读 `HEAD` 的树，所以**未提交**的工作区改动不在它射程内；
  那一路仍由金标准逐字节比对（LTS 硬拦 / main report-only）与 `tracked_dirty()`
  拒绝脏树导出兜着。
- 新线切出后忘记 `--pin-frozen` ⇒ 直接红（fail closed）。这是有意的：手册 §8 第 3 步
  因此必须把 pin 写成流程的一部分（本轮已同步手册）。
- `LTS-v3.0` 上的 `check-lts-api-surface.py` 目前是**旧版本**（守卫脚本只在 main 演进）。
  这条闸要在冻结线上生效，得和 `surface-policy.yml` 一起走"cherry-pick 进 LTS"那笔账
  （交接文档 §7 第三条，仍需人工按手册 §1/§7 走一次）。
- 反证（规则 18）：临时分支上给 `docs/api-surface/v3.0/index.ts` 追加一行
  `export * from "./types/qaqh/HandEditedIntoTheLock";` 并提交 ⇒
  `python scripts/check-lts-api-surface.py --skip-export` **exit 1**，报
  `golden tree docs/api-surface/v3.0 is not the pinned lock: HEAD=91fd3e12… pinned=e0fc731c…`；
  分支已删除、工作区已复原（实测 `git status` 无残留）。
- 分类闸的射程缺口本身**没有**被本 ADR 关掉（本 ADR 管的是"谁能动锁"，不是"谁改了契约"）：
  实测 `check-surface-change-policy.py` 对 `5880a867` 仍是 `规则不适用` exit 0。
  分工是有意为之：LTS 侧由 `lts-api-guard.yml` 的逐字节比对硬拦（改了代码就红），
  main 侧允许代码漂移（report-only），而"动锁"由本 ADR 的登记检查在两条线上都硬拦。
- 本 ADR 自身那笔改动（只动 `snapshots/`）也被分类闸机器判过：
  `金标准改动 1 个文件（导出物 0 / 快照 1 / wire 实变 0）⇒ 需要标记 [guard-tightening]`
  带标记 exit 0；把同一改动集的标记去掉（临时分支 amend）⇒ **exit 1**
  `缺少必需标记 [guard-tightening]`。第三类判据不是文案承诺。

## 落地清单

- [x] 守卫新增 `compare_frozen_locks()` / `--pin-frozen`，并登记 `docs/api-surface/v3.0`
- [x] 手册 §1（"main 上这条红是设计状态"）与 §8（切线第 3 步补 `--pin-frozen`）同步
- [ ] 仓库所有者复核本 ADR；复核时一并决定：ADR-0007 §决策 5 的"金标准目录出现非导出物
      新文件即红"是否还需要单独实现（锁身份检查已是它的严格超集，重复实现反而多一处定义）
- [ ] 与 `surface-policy.yml` / `CODEOWNERS` 一起进 `LTS-v3.0`（那笔账见交接 §7）
