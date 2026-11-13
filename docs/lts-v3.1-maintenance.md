# LTS v3.1 维护发版说明书

> 适用分支:`LTS-v3.1` · 起点版本:`v3.1.0`(tag 于 main,与 LTS 分支同源;切线决策见 ADR-0010)
> 金标准:`docs/api-surface/v3.1/`(机器写,`--rebaseline` / `--pin-frozen`)
> 契约代际:`ringing-commands-9` / `ringing-session-2`(切线时沿用 main 已 bump 的值,见 §7.1)
> 配套 CI:`.github/workflows/release.yml`(发版,三平台)、`sdk-contract.yml`(契约校验)、
> `lts-api-guard.yml`(冻结守卫)、`surface-policy.yml`(改动分类)
> 决策台账:`docs/adr/`——本文写**怎么做**,`docs/adr/` 写**为什么这样定、以及哪些方案被否**。
> 上一代锁的历史登记在 `docs/lts-v3.0-maintenance.md`(v3.0 金标准树保留至 EOL,只读)。

## 0. 分支模型:kernel / user-space

| 分支 | 角色 | 内部实现 | API 面(RingingV2 + SDK 契约) |
|---|---|---|---|
| `main` | 开发快车 | 随便破坏、重构、删除 | 允许演进(revision bump) |
| `LTS-v3.1` | 用户空间依赖的稳定面 | 可修 bug、可性能优化、可加私有代码路径 | **冻结**:行为与 `v3.1.0` release 完全一致 |

前端(qaqh-electron / qaqh-sdk)只允许依赖 `LTS-v3.1` 的 tag 产物;main 上的任何破坏性改动对前端不可见,直到下一条 LTS 线确立。
不变量:**`main` ⊇ `LTS-v3.1`**(宪规 19);切线提交 ff-merge 回 main 后打 tag。

## 1. API 面冻结纪律(发版前必读)

**冻结的定义**——LTS 分支上,以下内容禁止变更行为、字段、语义:

1. **HTTP 端点集合**:`/ringing/v2/*` 全部路由 + `/health` `/activity` `/control/v1/*`
   —— 由两张数据表登记并驱动 `build_router`:`qaqh_ringing::RINGING_V2_HTTP_ROUTES`
   与 `axum_impl::CONTROL_PLANE_ROUTES`。加路由的唯一途径是往表里写一行;
   守卫 lint 禁止 `axum_server/**` 出现字面量 `.route("…")`/`.nest(`/`.merge(`。
   冻结粒度含 HTTP 方法(`snapshots/routes.txt`,机器重切,勿手改)。
2. **信封与版本闸门**:`RingingV2CommandEnvelope` 字段、`contract_revision` 的取值
   (`ringing-commands-9` / `ringing-session-2` 在本 LTS 生命周期内**保持原值不动**)
3. **类型化方法表**:`TYPED_METHODS`(`qaqh-runtime/src/ringing/service_methods*.rs`)中每个方法的
   名称、params/result 类型形状、MethodKind、错误语义
4. **命令与事件面**:`RingingCommand`、`RingingV2Event` 各变体的字段与判别方式
5. **SDK 契约哈希**:偏离必须先在 §7 登记后才允许提交;additive 且不动 `contract_revision`
   的偏离可记在当前锁内,其余一律开新 LTS 线(v3.2)。

**允许的改动**:bug 修复、内部重构(不触及上述面的)、新增仅在内部使用的代码路径、测试。

**闸门速查**(各闸门实际比什么,别把纪律当成 CI):

| 闸门 | 实际比对对象 | 能抓到什么 |
|---|---|---|
| `lts-api-guard.yml` → `scripts/check-lts-api-surface.py` | HEAD 导出物 vs 仓内 `docs/api-surface/v3.1/` golden(逐字节,manifest 的 `source_revision`/`source_dirty` 除外)+ 路由表 + facade 清单 + 锁身份(frozen-locks.json) | 实现改了 wire 但没同步 golden(或反之);就地偷换锁 |
| `sdk-contract.yml` | 同份 temp 导出自比 + 严格 TS 消费者 | 导出不确定/不可复现;前端可见字段被删 |
| `lts-api-guard.yml` → `scripts/check-frozen-ledger.py` | 本次改动文件集合 vs 本文 §7 新增条目 | 触碰冻结面却没登记 |
| `surface-policy.yml` → `scripts/check-surface-change-policy.py` | 金标准改动的 wire 视图三分类(refreeze / tightening / doc-only) | 谎报 doc-only;refreeze 混进冻结线 |

本地官方验证两条:
① `python scripts/check-lts-api-surface.py`(默认 `--against docs/api-surface/v3.1`);
② `python scripts/check-sdk-contract.py --typescript --release`。

**`main` 上金标准比对红是设计状态**(ADR-0007 §决策 4):`surface-policy.yml` 的
`Golden contract byte compare` 只在 target = `LTS-v3.1` 时硬拦,main 上 report-only。
唯一禁止的动作是改 `docs/api-surface/v3.1` 让它变绿——那棵树是冻结线持有的锁。
main 上攒着的 refreeze 差额记在 `docs/adr/0010-cut-lts-v3.1-line.md` 的后继账本(切 v3.2 时立新 ADR)。

锁身份登记(ADR-0011):`docs/api-surface/snapshots/frozen-locks.json`,唯一写法
`python scripts/check-lts-api-surface.py --pin-frozen`;守卫在 `--skip-export` 下也硬比 tree OID。

## 2. 标准补丁发版流程(LTS-v3.1)

```bash
# 0. 确认在 LTS 分支且与 origin 同步(禁 git checkout,用 switch/restore)
git switch LTS-v3.1 && git pull

# 1. 补丁修复提交后,同步版本戳(如 3.1.2026MMDD-g<sha>)
pwsh scripts/sync-version.ps1   # 先把 version.txt 改成本次补丁戳
#    sync 写 Cargo.toml + package.json;Cargo.lock 由任意 cargo 命令同步;
#    qaqh-backend.lock.json 的手工两字段(version/git_commit)一并改 —— 五处缺一不可

# 2. 验证契约(可选但强烈建议)
python scripts/check-sdk-contract.py --typescript --release

# 3. 走 PR(LTS-v3.1 开 Require PR + enforce_admins,直推被拒)
git switch -c feat/<slice> && git push -u origin feat/<slice>
gh pr create --base LTS-v3.1 --head feat/<slice> --title ... --body-file ...
#    必需检查:contract / freeze / change-policy 绿
gh pr merge <n> --merge

# 4. 干跑构建验证(不发版,三平台编一遍)
gh workflow run release.yml --ref LTS-v3.1

# 5. 打 tag 发布
git tag -a v3.1.<n> -m "QAQ-Harness LTS v3.1.<n>"
git push origin v3.1.<n>
#    → Release 工作流:三平台 --locked release 构建 → 打包 → 发 GitHub Release(Latest)
```

产物(自动生成,勿手传):

- `qaqh-daemon-v3.1.x-windows-x64.zip` / `qaqh-daemon-v3.1.x-linux-x64.tar.gz` /
  `qaqh-daemon-v3.1.x-macos-arm64.tar.gz`(daemon + version.txt + qaqh-backend.lock.json)
- `qaqh-sdk-contract-v3.1.x.zip`(Linux job 导出,`QAQH_SOURCE_REVISION` = tag 提交)

## 3. 紧急热修

分支保护对 admin 也生效(`enforce_admins=true`),热修**不能直推** `LTS-v3.1`:
流程同 §2,允许跳过步骤 2/4。热修 tag 仍必须走 `v3.1.x` 序号,禁止 `latest`/浮动 tag。
若热修动了导出物 → commit 必须带 `[contract-refreeze]`,而那**不允许进 LTS**:
说明它不是热修,是换锁,走 §8。

## 4. CI 矩阵速查

| 工作流 | 触发 | 作用 |
|---|---|---|
| `sdk-contract.yml` | PR / push(main、LTS-v3.1)/ 手动 | Rust 测试三件套 + 契约确定性 + 严格 TS 消费者 |
| `lts-api-guard.yml` | push / PR → `LTS-v3.1` | 金标准逐字节比对 + 路由表/门面快照 + 锁身份(硬闸) |
| `surface-policy.yml` | PR / push(main、LTS-v3.1) | 改动分类闸;金标准比对 main report-only、LTS 硬拦 |
| `release.yml` | 推 `v*` tag / 手动 dispatch | 三平台 `--locked` release 构建、打包、发 Release |

外部依赖钉子:`qaqtam/mutil-ai` @ `b37ca25f23350a8c6cb78bc69fda023f2cb809aa`。
LTS 生命周期内**禁止**升级这个 pin;升级属 main 的权利。

## 5. 故障手册

与 v3.0 线同源,见 `docs/lts-v3.0-maintenance.md` §5(lock 不同步 exit 101、dirty checkout
拒绝、TS2353 漂移、tag 事件 500 重推)。macOS 新增注意:runner 为 arm64,产物名
`macos-arm64`;如需 Intel 产物另加 `macos-15-intel` 矩阵项(当前未启用)。

## 6. 发版 checklist

- [ ] 触碰 §1 冻结面的提交都已在 §7 登记(change-policy 绿 = 机器形式)
- [ ] `version.txt` / Cargo.toml / package.json / Cargo.lock / qaqh-backend.lock.json 五处同步
- [ ] sdk-contract 在 LTS-v3.1 绿
- [ ] (标准流程)dispatch 干跑三平台绿
- [ ] tag 已推,Release 页面出现且标记 Latest,三平台资产 + 契约 zip 齐全
- [ ] 前端仓库把依赖 pin 到新 tag(契约 zip 的 `QAQH_SOURCE_REVISION` 与 tag 提交一致)

## 7. 锁内已批准的契约修订登记

> 判据与 v3.0 线同:触碰冻结面 ⇒ 必须同改动新增一条 `### 7.<n> …`(check-frozen-ledger.py 执法)。
> 换锁 = bump `contract_revision` + 重切 tag,属开新 LTS 线(§8),不得记在本节。

### 7.1 2026-11-13 · 切线登记 · v3.1.0 吸收 main 攒下的 refreeze 批

- **变更**:从 main 切出 `LTS-v3.1`,机器重切金标准 `docs/api-surface/v3.1/`
  (exporter `export --release` + `--rebaseline` + `--pin-frozen`,同一提交)。
  吸收的 wire 实变批(逐条出处见 ADR-0010 §攒着的 refreeze 清单):
  PermissionRequestDetails 入契约 + approvals 显式失败、Skills 子系统裁剪(−6 文件 + schema 7→8 迁移)、
  流式参数预览字段(target_files/args_complete)、conversation.truncate + 拒绝码、
  子代理配置面退役(ConfigDto/ConfigPatch 删 subagent 段 + 两型)、Goal 模式(goal.* 命令 + GoalView + schema 9)、
  Handoff 内核 fork(HandoffSealed + 血缘导出)、exec_session.output 区间读(+2 DTO)、
  Team inbox InterAgentActor 寻址统一 + FactPayload::InterAgentCommunication 死变体退役(ADR-0019)。
- **兼容性判定**:整批为 **contract-refreeze**(增删产物 + 字段类型变),故不进 v3.0 锁,开新线。
- **`contract_revision`**:`ringing-commands-9`(ADR-0015 §已决 2 所有者拍板,随 Goal/truncate 已 bump)/
  `ringing-session-2`(事件/读模型纯增量,沿 ADR-0010 对 additive 的口径,不 bump)。
  切线本身**不再新 bump**——取值在 main 上已由逐条所有者决策定死。
- **闸门放行**:golden 与实现同提交 ⇒ `freeze` 绿;本条即 `change-policy` 留痕;
  `contract` 见 PR/tag CI。v3.0 金标准树一字节未动,其锁身份登记原样保留。
- **前端连带**:旧 pin v3.0 契约的客户端对 v3.1 daemon 对 `-7`/`-8` 命令代际一律 426/428 拒连
  (设计语义,不做半兼容);前端须 pin v3.1.0 契约 zip 并同批发布(ADR-0010 §待决 3 的 500 文案
  由前端仓自行确认,不阻塞后端发版——所有者 2026-11-13 发版指令)。

## 8. 换锁:开新 LTS 线(v3.2,`contract-refreeze` 的唯一去处)

流程与 v3.0→v3.1 同(手册模板):先写 ADR(新 revision 取值、被破坏 capability、迁移窗口)→
main 落 `[contract-refreeze]` → `git switch -c LTS-v3.2 main` → 机器重切
`docs/api-surface/v3.2` + `--rebaseline` + `--pin-frozen`(同一提交)→ 改指向
(`lts-api-guard.yml` branches、`check-lts-api-surface.py` 默认 `--against`、
`check-frozen-ledger.py` LEDGER_FILE、`surface-policy.yml`/`sdk-contract.yml` branches 与 target、
本文让位给 `docs/lts-v3.2-maintenance.md`)→ 新线分支保护 → ff-merge 回 main → 切 tag。