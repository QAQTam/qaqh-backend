# LTS v3.0 维护发版说明书

> 适用分支:`LTS-v3.0` · 起点版本:`v3.0.0`(tag 于 main @ `7fe35d96` 切出,与 LTS 分支同源)
> 配套 CI:`.github/workflows/release.yml`(发版)、`.github/workflows/sdk-contract.yml`(契约校验)、
> `.github/workflows/lts-api-guard.yml`(冻结守卫)
> 决策台账:`docs/adr/`——本文写**怎么做**,`docs/adr/` 写**为什么这样定、以及哪些方案被否**。
> 已采纳但未实施的闸门见 ADR-0003～0008;引用时注意其状态,勿把"已采纳(未实施)"读成"已完成"。

## 0. 分支模型:kernel / user-space

引入 kernel 思想后,两条分支的职责严格二分:

| 分支 | 角色 | 内部实现 | API 面(RingingV2 + SDK 契约) |
|---|---|---|---|
| `main` | 开发快车 | 随便破坏、重构、删除 | 允许演进(revision bump) |
| `LTS-v3.0` | 用户空间依赖的稳定面 | 可修 bug、可性能优化、可加私有代码路径 | **冻结**:行为与 `v3.0.0` release 完全一致 |

前端(qaqh-electron / qaqh-sdk)只允许依赖 `LTS-v3.0` 的 tag 产物;main 上的任何破坏性改动对前端不可见,直到下一条 LTS 线确立。

## 1. API 面冻结纪律(发版前必读)

**冻结的定义**——LTS 分支上,以下内容禁止变更行为、字段、语义:

1. **HTTP 端点集合**:`/ringing/v2/*` 全部路由 + `/health` `/activity` `/control/v1/*`
   —— 两者都由**数据表**登记并驱动 `build_router`:`qaqh_ringing::RINGING_V2_HTTP_ROUTES`
   与 `axum_impl::CONTROL_PLANE_ROUTES`。加路由的唯一途径是往表里写一行(表驱动 + 缺 arm 启动
   panic);守卫 lint 禁止 `axum_server/**` 出现字面量 `.route("…")`/`.nest(`/`.merge(`。
   冻结粒度含 HTTP 方法(见 `snapshots/routes.txt`,由 `--rebaseline` 机器重切,勿手改)。
2. **信封与版本闸门**:`RingingV2CommandEnvelope` 字段、`contract_revision` 的取值
   (`ringing-commands-7` / `ringing-session-2` 在整个 LTS 生命周期内**保持原值不动**)
3. **类型化方法表**:`TYPED_METHODS`(`qaqh-runtime/src/ringing/service_methods*.rs`)中每个方法的
   名称、params/result 类型形状、MethodKind、错误语义
4. **命令与事件面**:`RingingCommand`、`RingingV2Event` 各变体的字段与判别方式
5. **SDK 契约哈希**:`qaqh-contract-export` 的 `contract_hash` 相对 `v3.0.0` release 内契约 zip **不得变化**
   (相对 v3.0.0 的偏离必须先在 §7 登记后才允许提交;additive 且不动 `contract_revision` 的偏离可记在
   当前锁内,其余一律开新 LTS 线)。**"偏离必须留痕"这半句已由 `change-policy` 作业机器执法**
   (见下表);至于"偏离是否允许",仍由本节判据 + §7 登记内容决定。

**允许的改动**:bug 修复、内部重构(不触及上述面的)、新增仅在内部使用的代码路径、测试。

**改动是否踩线的判定法**——先分清各闸门实际比什么(别把纪律当成 CI):

| 闸门 | 实际比对对象 | 能抓到什么 |
|---|---|---|
| `lts-api-guard.yml` → `scripts/check-lts-api-surface.py` | HEAD 导出物 vs **仓内** `docs/api-surface/v3.0/` golden(逐字节,`manifest.json` 的 `source_revision`/`source_dirty` 除外)+ 路由表 + facade 清单 | 实现改了 wire 但没同步 golden(或反之) |
| `sdk-contract.yml:48-49` | `export --out $T` 后 `check --against $T`,即**同一份** temp | 导出不确定/不可复现 |
| `scripts/check-sdk-contract.py --typescript --release` | 由导出物自算 `contract_hash` 校验 manifest 自洽 + 严格 TS 消费者编译 | 契约自相矛盾、前端可见字段被删(表现为 `error TS2353`) |
| `lts-api-guard.yml` → `scripts/check-frozen-ledger.py` | 本次改动的**文件集合** vs `docs/lts-v3.0-maintenance.md` 的 §7 新增条目 | 触碰前端可见冻结面(docs/api-surface、ringing-contract、ringing/src/v2、command.rs、qaqh-client、contract-export)却没有登记 |
| `surface-policy.yml` → `scripts/check-surface-change-policy.py` | 金标准改动的 **wire 视图** 三分类(refreeze / tightening / doc-only,ADR-0009) | 谎报 doc-only 的 wire 实变;refreeze 混进冻结线 |

本地两条官方命令都跑 ——
① `cargo run -p qaqh-contract-export -- check --against docs/api-surface/v3.0`(金标准比对。自 `52a0d0bb`
起 `compare_trees` 豁免 `snapshots/`、`manifest.json` 走切除 `source_revision`/`source_dirty` 的语义比对,
且 `.gitattributes` 已把金标准目录钉成 `eol=lf`,故 Windows 与 Linux 同样可用,本机实测 exit 0);
② `scripts/check-sdk-contract.py --typescript --release`(产物确定性 + 独立严格 TS 消费者)。

**仍然没有任何 CI 步骤把 `contract_hash` 与 `v3.0.0` release 资产比对**(那条比的是影子不是成因:一句 doc
注释也能让 hash 漂移,见 §7.2)。所以 §1.5 的可执行形式是:**碰冻结面 ⇒ 必须留下 §7 登记**(`change-policy`
红),而不是"hash 不许变"这种没人比的口号。LTS 上不允许用"顺手改夹具"的方式让它变绿(夹具只能修编译性破损,
不得改描述被测行为的断言);`change-policy` 自身的判据由它的 `--self-test` 钉住——把 `FROZEN` 前缀删短或把
§7 标题正则放宽,那一步会先红。

**漂移事故参照**:model-config 重构删除 `SessionSettingsPatch.profile` 时,TS 严格消费者在 CI 抓到漂移
—— LTS 上这类漂移 = 发布阻断,必须回退实现而非改夹具。

**机器闸门**:`.github/workflows/lts-api-guard.yml`(push LTS / PR → LTS 触发)运行
`scripts/check-lts-api-surface.py`,把 HEAD 的契约导出物与 `docs/api-surface/v3.0/` 金标准逐字节比对
(manifest 中随提交变化的 `source_revision/source_dirty` 除外),并冻结 HTTP 路由表与 qaqh-client facade 清单。
CI 红 = 拒绝该改动;金标准只允许在两种情况下再生成:① 新 LTS 线(如 v3.1)切出时;
② LTS 上明确批准的契约修订(此时它已不是"冻结"而是"换锁"):additive 且不动 `contract_revision` 的偏离
留在当前锁内、随 §7 登记提交即可;要删/改字段、改语义或 bump `contract_revision`,那是开新 LTS 线(v3.1)。
再生成命令:`QAQH_SOURCE_REVISION=$(git rev-parse HEAD) QAQH_SOURCE_DIRTY=0 cargo run -p qaqh-contract-export -- export --out <tmp> --release`,
随后把产物复制进金标准目录、连同 `snapshots/routes.txt`、`snapshots/facade.txt` 一并提交。

**`main` 上这条比对红是设计状态，不是待修缺陷**（ADR-0007 §决策 4）：
`surface-policy.yml` 的 `Golden contract byte compare` 只在 target = `LTS-v3.0` 时硬拦，
在 main 上是 `continue-on-error`（**job 绿、step 明细也绿，只有日志写着**
`##[error]Process completed with exit code 1` —— 实测于 run `42798374172`。
所以验收新闸门要看 `gh run view <id> --log`；只看 job 或 step 的绿灯都会骗人，
规则 18 的原表述在此修正）。
任何落在 main 的 `contract-refreeze`（新增产物、字段变化）都会让它红，直到按 §8 切新线
并由机器重切金标准。此时**唯一禁止的动作**是改 `docs/api-surface/v3.0` 让它变绿：那棵树
是冻结线持有的锁。main 上目前攒着的 refreeze 差额记在
`docs/adr/0010-cut-lts-v3.1-line.md`（该 ADR 同时列着切线前必须由人拍板的 revision 取值）。

"重切一下它就绿"这条路已被 ADR-0011 封死：每条冻结线的金标准树**身份**必须登记在
`docs/api-surface/snapshots/frozen-locks.json`（唯一写法
`python scripts/check-lts-api-surface.py --pin-frozen`），守卫每次运行都拿
`HEAD:<金标准目录>` 的 git tree OID 比对登记值，且这段**在 `--skip-export` 下也跑** ——
而 `surface-policy.yml` 在 main 上硬跑守卫，只有产物逐字节比对才是 report-only。
于是：改了代码 ⇒ 那条红留着（允许的演进）；改了锁不补登记 ⇒ 守卫 exit 1（拒绝）。
补登记本身只动 `snapshots/`，属 `guard-tightening`，且 `docs/api-surface/**` 在
CODEOWNERS 里 ⇒ 必须 owner 评审。

## 2. 标准补丁发版流程(LTS-v3.0)

```bash
# 0. 确认在 LTS 分支且与 origin 同步
git checkout LTS-v3.0 && git pull

# 1. 补丁修复提交后,同步版本戳(如 v3.0.1)
pwsh scripts/sync-version.ps1   # 先把 version.txt 改成 3.0.2026MMDD-N 之类的补丁戳
#    注意:sync 会写 Cargo.toml + package.json;Cargo.lock 里的 workspace 版本戳
#    由任意一次 cargo 命令自动同步,提交前确认 `git status` 把 lock 一并纳入

# 2. 验证契约未冻结区被误伤(可选但强烈建议)
just sdk-check

# 3. 走 PR(不再直推:LTS-v3.0 开了 Require PR + enforce_admins,直推按平台语义会被拒)
git switch -c feat/<slice> && git push -u origin feat/<slice>
gh pr create --base LTS-v3.0 --head feat/<slice> --title ... --body-file ...
#    三项必需检查必须绿:contract(sdk-contract.yml)、freeze(lts-api-guard 内容一致)、
#    change-policy(lts-api-guard 留痕)。required_approving_review_count=0 —— 单人维护者
#    凑不出"非作者的 code owner",评审人头这道门已在 §7.3 撤掉,执法点改到上述三项。
gh pr merge <n> --merge

# 4. 干跑构建验证(不发版,两平台编一遍)
#    GitHub → Actions → Release → Run workflow → 选 LTS-v3.0
#    或:gh workflow run release.yml --ref LTS-v3.0

# 5. 打 tag 发布
git tag -a v3.0.1 -m "QAQ-Harness LTS v3.0.1"
git push origin v3.0.1
#    → Release 工作流自动:双平台 --locked release 构建 → 打包 → 发 GitHub Release(标记 Latest)
```

产物(自动生成,勿手传):

- `qaqh-daemon-v3.0.x-windows-x64.zip` / `qaqh-daemon-v3.0.x-linux-x64.tar.gz`(daemon + version.txt + qaqh-backend.lock.json)
- `qaqh-sdk-contract-v3.0.x.zip`(Linux job 导出,`QAQH_SOURCE_REVISION` = tag 提交)

## 3. 紧急热修

**2026-11-10 起分支保护对 admin 也生效**（`enforce_admins=true`），所以热修**不能直推** `LTS-v3.0`：

```bash
git switch -c hotfix/v3.0.x-plus1 origin/LTS-v3.0
#   改实现 → 推分支 → 开 PR 到 LTS-v3.0
#   过闸要求：contract + freeze 绿 + 1 个批准 + 会话已 resolve
#   若这次热修动了导出物 → commit 里必须带 [contract-refreeze]，而那**不允许进 LTS**：
#   说明它不是热修，是换锁，得走 §8 的新线流程
```
流程同上,但允许跳过步骤 2/4（本地 sdk-check 与 dispatch 干跑）；步骤 3 的 CI 绿改成由 PR 的
required checks 保证。热修 tag 仍必须走 `v3.0.x` 序号,禁止 `latest`/浮动 tag。

真要紧急绕过（不推荐，会在保护审计里留痕）：
`gh api --method DELETE repos/QAQTam/qaqh-backend/branches/LTS-v3.0/protection/enforce_admins`
用完立刻 PUT 回来（见 §7 底部命令）。

## 4. CI 矩阵速查

| 工作流 | 触发 | 作用 |
|---|---|---|
| `sdk-contract.yml` | PR / push(main、LTS-v3.0)/ 手动 | Rust 测试三件套 + 契约确定性 + 严格 TS 消费者 + 契约哈希锁定 |
| `lts-api-guard.yml` | push / PR → `LTS-v3.0` | 金标准逐字节比对 + 路由表/门面快照(硬闸,仅冻结线) |
| `surface-policy.yml` | PR / push(main、LTS-v3.0) | **改动分类闸**:refreeze / tightening / doc-only 三判 + 路由登记处 lint + 门面;金标准比对在 main 上 report-only、在 LTS 上硬拦 |
| `release.yml` | 推 `v*` tag(发布)/ 手动 dispatch(干跑) | 双平台 `--locked` release 构建、打包、发 Release |

`surface-policy.yml` 的判据是**改动落点 + wire 视图**,不是人写的标签文案。三类(仓库已建好对应 label):

| 标记 | 何时 | 能否进 `LTS-v3.0` |
|---|---|---|
| `contract-refreeze` | wire 实变:增删产物、类型字段变化 | ❌ 只能开新线 + bump revision |
| `guard-tightening` | 只动 `snapshots/*`,导出物逐字节不变 | ✅ |
| `doc-only-refresh` | 导出物字节变了但去掉注释后 **wire 视图相同**(如给 DTO 加 doc 注释,`contract_hash` 会跟着变) | ✅ 且不 bump revision |

标记写在 commit subject/body(`[doc-only-refresh]`)或 PR label。**是否真属 doc-only 由脚本自己算**
(它对每个改动产物做去注释比较),谎报会被 `artifact added` / `wire-differs` 列表当场戳穿 —— 见 ADR-0009。
实测:引入整份金标准的 `b96f2b72` 判红;只改 `routes.txt` 的 ADR-0003 提交自动判 `guard-tightening` 放行;
LTS 上那次注释重切(`ac54e508`+`4c3a26be`)判 `doc-only-refresh`。

外部依赖钉子:`qaqtam/mutil-ai` @ `b37ca25f23350a8c6cb78bc69fda023f2cb809aa`(2026-11-11 随 prompt-cache 收口升级;旧 pin `4387f74d` 经 merge-base 实测为其祖先)。
LTS 生命周期内**禁止**升级这个 pin(path 依赖行为随它变);升级属 main 的权利。

## 5. 故障手册(本 SDK 实战踩过的坑)

| 症状 | 根因 | 处置 |
|---|---|---|
| workflow run 0 job 秒败,注解 `Internal server error. Correlation ID: …` | GitHub 服务端处理 push/tag 事件的偶发 500 | 重推 tag:`git push origin :refs/tags/<t> && git push origin <t>` |
| `cargo build --locked` exit 101:`cannot update the lock file` | Cargo.lock 与 manifest 不同步(典型:版本戳只改了 Cargo.toml 没进 lock) | 本地跑任意 cargo 命令让 lock 同步,`git diff Cargo.lock` 确认无依赖漂移后提交 |
| sdk-contract 报 `refusing release validation from a dirty checkout` | 前序 cargo 步骤改写了工作区(通常同上,lock 版本戳陈旧) | 修 lock 并提交;**不要**绕过 dirty 检查 |
| `error TS2353: … does not exist in type …` | 契约面漂移(实现删了前端可见字段) | LTS:回退实现;main:确认属预期演进后修夹具并 bump revision |
| Release 资产缺失/双 job 竞态 | 理论竞态 | 工作流为 build→artifact→publish 三段式,正常不会发生;重跑 release job 即可 |

## 6. 发版 checklist

- [ ] `git log v3.0.<n-1>..LTS-v3.0` 里触碰 §1 冻结面的提交都已在 §7 登记(未登记 = 本次发版阻断)
- [ ] `change-policy` 作业在 PR 上为绿(它就是上一条的机器形式;它红 = 有冻结面改动没留痕)
- [ ] `contract_hash` 与上一版 release 的契约 zip 一致;不一致时 §7 必须有对应条目,且偏离是 **additive + 未 bump `contract_revision`**
- [ ] §7 登记条目里写清了「哪道闸门放行、为什么放行」(golden 同提交即 `freeze` 绿),别让下个维护者以为 CI 兜住了语义
- [ ] `version.txt` 已改为本次补丁戳,Cargo.toml / package.json / Cargo.lock / qaqh-backend.lock.json 全部同步
- [ ] sdk-contract 在 LTS-v3.0 绿
- [ ] (标准流程)dispatch 干跑双平台绿
- [ ] tag 已推,Release 页面出现且标记 Latest,资产齐全
- [ ] 前端仓库把依赖 pin 到新 tag(契约 zip 里的 `QAQH_SOURCE_REVISION` 与 tag 提交一致)

## 7. 锁内已批准的契约修订登记

> 「换锁」的判据不是补丁号进位(3.0.0→3.0.1 只是版本号),而是这条锁的参照物有没有被改动后仍留在锁内。
> 换锁 = bump `contract_revision` + 重切 tag,那属于开新 LTS 线(§1.2/§42);登记在案的 additive 偏离则
> 留在当前锁内,以 golden 目录为锁内新基线。**换锁 ≠ 破坏性改动**:删字段/改语义是破坏性,加可选字段不是。
>
> 从 §7.3 起,本节不再只是纸面纪律:`change-policy` 作业会比对"触碰冻结面"与"本节新增条目",缺一即红。

### 7.1 2026-11-10 · 随 `v3.0.1` 携带 · 事件面加可选行差估算字段

- **变更**:`SessionEvent::ToolArgumentsDelta` 增 `lines_added` / `lines_removed`(累计行差估算)。
- **兼容性判定**:纯 additive。未删字段、未改任何既有字段语义、未改事件判别方式;两字段
  `#[serde(default, skip_serializing_if = "Option::is_none")]`,旧客户端报文里根本不出现这两个键。
- **`contract_revision` 未 bump**(仍 `ringing-commands-7` / `ringing-session-2`,`qaqh-ringing/src/v2/types.rs:39,44`):
  客户端对它的校验是严格相等,bump 会让所有存量前端当场拒连——一个"先顶一下"的补丁 tag 承担不起。
- **基线**:`docs/api-surface/v3.0/` 已随实现同提交再生成(变化面:
  `cs/SessionEvent.cs`、`cs/SessionEventFrame.cs`、`types/qaqh/SessionEvent.ts`、`types/qaqh/SessionEventFrame.ts`、
  `manifest.json`;路由表与 facade 未动)。`lts-api-guard` 以 golden 为准,故本条不会变红——**这是登记而非 CI 放行**。
- **消费口径**:估算数的是已到达的**参数文本**行数,不是实际改动行数;终态 `ToolFinished.code_delta`
  才是盘上真 diff(本次一并把 code_delta 从参数文本口径改为展示投影真 diff 单一事实源)。
  前端把估算当进度条,不得把 `null` 渲染成 `+0 −0`。
- **出口**:任何后续要删/改这些字段、改语义、或 bump revision 的变更 → 开 `v3.1` 线,不得再记在本节。

### 7.2 2026-11-10 · 已合入 `4c3a26be` · 事后追认:doc 注释驱动的 hash 漂移

- **事实**:`4c3a26be`(chore(contract): regenerate golden for ModelCapabilities layering docs)只改了两个
  文件——`types/qaqh/ModelCapabilities.ts` 增加 13 行**注释**(源自 `ac54e508` 给
  `crates/qaqh-types/src/provider.rs` 加的结构体文档),`manifest.json` 的 `contract_hash` 随导出物变化。
  提交信息自己写明 "Shape unchanged - docs only"。C# 侧产物未变。
- **性质**:**不是**字段/语义变更,`ModelCapabilities` 的形状(`tool_calls`/`image_input`/
  `native_web_search`/`reasoning`)一字未动 → 对客户端零影响,不需要任何前端改动。
- **为什么要追认**:它直推进了当时已开保护的 `LTS-v3.0`,触碰了 `docs/api-surface/**` 却无登记。
  `contract_hash` 变了,而 §1.5 旧文面把它写成"不得变化"——按旧文面这次属违规,按实际影响这次属噪声。
  这正是把纪律建在影子上而非成因上的后果,已在 §1.5 改写为"碰冻结面必留痕"并由 `change-policy` 执法:
  同样内容今天会**强制**要求一条登记才允许合并。
- **登记编号说明**:`§7.1` 在前、`§7.2` 在后按"入账顺序"排列而非按提交时间;`4c3a26be`(16:45)早于
  `§7.1` 对应的 PR #16(20:29),但追认条目一律按登记时刻排在后。

### 7.3 2026-11-10 · 锁的执法方式变更(B 方案,无代码影响)

- **变更**(仓库设置,不进 git 树,故本条是唯一留痕处):`LTS-v3.0` 分支保护由
  `required_approving_review_count=1` + `require_code_owner_reviews=true` + `enforce_admins=true`
  改为 `count=0` + `code_owner=false`,**`enforce_admins` 与 strict `contract`/`freeze` 保留**。
- **为什么必须改**:唯一有 write 权限的账号是 `QAQTam`,而 PR 作者也是 `QAQTam`;GitHub 硬规则禁止作者批准
  自己的 PR(owner/admin 无例外),`enforce_admins=true` 又关掉了 admin 绕过,仓库级 auto-merge 也未开启 ⇒
  合格批准人集合为空,**任何 PR 都进不来**(PR #16 当时 `reviewRequests: []`,平台自己也算不出人)。
- **为什么不加第二个 code owner 了事**:单人维护下,"多一个人头"要么由 bot 自动批准(那实质就是机器执法,
  不如把判据写成可测的断言),要么永远凑不齐(那就是现在这个死锁)。所以执法点迁到
  `contract` + `freeze` + 新增的 `change-policy`。
- **为什么保留 `enforce_admins=true`**:比原提案更严。它让 admin 也不能直推绕开三项检查——§7.2 那类
  "绕过锁的推送"从此结构性不可能。代价:§2 的发版流程必须走 PR(已改写)。
- **注意**:`.github/CODEOWNERS` 只存在于默认分支 `main`,`LTS-v3.0` 树上没有这个文件;`main` 那份把
  冻结面与守卫脚本全部指向 `@QAQTam`,因此"Require review from Code Owners"在 main 上会复现同一个死锁。
  主线若采纳同一判据,应把 code-owner 评审换成可自动判定的断言。
- **兼容性**:零 wire 影响,不涉及 revision,不影响任何已发布契约。

### 7.4 2026-11-11 · 随 v3.0 收口 PR #21 · client 读面错误信封还原(无 wire 影响)

- **变更**:`qaqh-client::Client::query` 非 2xx 不再丢弃响应体,改复用 `service::response_error`
  还原 daemon 的 `{code,message}` 信封(与 `service_call` 同源);解析不出才退回 `Http`。
- **兼容性判定**:纯客户端错误投影改进,additive。HTTP wire、方法签名、facade 清单均未变
  (`freeze` 实测绿);结构化错误下消费方从裸 `Http` 变为 `Api`(多拿 code/message),只读 status
  的消费方不受影响。
- **`contract_revision` 未 bump**(仍 `ringing-commands-7` / `ringing-session-2`)。
- **闸门放行**:`freeze` 绿(导出物/路由/facade 不变);`change-policy` 以本条留痕;`contract` 见 PR CI。
- **背景**:前端 browser-control 文本拼接器把 config.toml 的 headers 双写致 `config.load` 400,
  根因文案(TOML duplicate key)被 `query()` 吞成 `code:null`。本修复让诊断到达前端;前端拼接器
  根治(daemon 结构化写口)留 v3.1。同 PR 的 ctx 页脚/pwsh UTF-8 修复不触冻结面前缀,无需登记。

### 7.5 分支保护基线(2026-11-10 开启的历史快照)

> **取代说明**:评审人头与 code-owner 两行已被 §7.3 取代(count=0、code_owner=false);
> `change-policy` 也已补进 required contexts(当前实测 contexts=[contract, freeze, change-policy])。
> 下表保留其余基线事实(enforce_admins、strict、禁 force push 等),"两个缺口"均已落地。

经所有者授权，已用 `gh api PUT /repos/QAQTam/qaqh-backend/branches/LTS-v3.0/protection` 开启并回读确认：

| 项 | 值 | 说明 |
|---|---|---|
| Require a pull request before merging | ✅（1 个批准） | **直推冻结线这条路从此关闭** —— 本轮"两个 worker 各修同一个缺陷并 merge 撞车"的那个入口 |
| Require review from Code Owners | ✅ | 见下方"尚未生效的一半" |
| Require status checks | ✅ `contract`、`freeze` | context 名是**从 API 实测取的**（不是猜的）：`sdk-contract.yml` 的 job 名 `contract`、`lts-api-guard.yml` 的 job 名 `freeze` |
| Strict（分支必须先与 LTS 同步） | ✅ | 两个 worker 并发时防"基于陈旧分支合入" |
| Require conversation resolution | ✅ | — |
| Allow force pushes / deletions | ❌（禁用） | 冻结线不可改写历史 |
| **Enforce for admins** | ✅ **true**（2026-11-10 第二次收紧） | admin 也**不能直推冻结线**，热修改走 §3 的 PR 路径 |

#### 两个**故意留着**的缺口（不要误以为全通了）

1. `change-policy`（即 `surface-policy.yml` 的 job）**没有**加进 required contexts ——
   因为 workflow 文件是从**被推的那个 ref** 读取的，而 `surface-policy.yml` 目前只在 `main` 上。
   现在就加，LTS 的每个 PR 会永远停在 "Expected"，等于把冻结线焊死。
   **等 `surface-policy.yml` + `.github/CODEOWNERS` 进了 LTS，再把这个 context 补上**：
   `gh api POST .../protection/required_status_checks/contexts --input '{"contexts":["change-policy"]}'`
2. 同理，`Require review from Code Owners` 已勾但 `.github/CODEOWNERS` 还不在 LTS ref 上 ⇒
   当前对 LTS 的 PR 是"无匹配 owner → 不触发 code-owner 要求"。第 1 条落地时这条自动一起生效。

`main` **未加保护**（快车，规则见 `docs/adr/0001` §决策）。


## 8. 换锁：开新 LTS 线（`contract-refreeze` 的唯一去处）

§1 说"换锁不是冻结"，但一直没写怎么走 —— 这一节就是那条路。凡动导出物（`contract_hash` 会变）
的改动只能落到这里：

1. **先写 ADR**（模板 `docs/adr/README.md`），至少含：新 `contract_revision` 值、被破坏的
   capability 清单、迁移窗口、ADR-0005 配对测试结论。无 ADR 的 refreeze 不批。
2. **先在 `main` 上落**：commit 带 `[contract-refreeze]`，CI 绿。
3. **切线**：`git switch -c LTS-v3.1 main`，重切金标准（机器写，不手改）：
   ```bash
   QAQH_SOURCE_REVISION=$(git rev-parse HEAD) QAQH_SOURCE_DIRTY=0 \
     cargo run -p qaqh-contract-export -- export --out docs/api-surface/v3.1 --release
   python scripts/check-lts-api-surface.py --against docs/api-surface/v3.1 --rebaseline
   # 同一个提交里把新线的锁身份也机器写掉（ADR-0011）：--pin-frozen 从工作区算 tree OID，
   # 不需要"先提交再补一次锁"（那中间的一次 CI 必然红在 not pinned 上）。
   python scripts/check-lts-api-surface.py --against docs/api-surface/v3.1 --pin-frozen
   ```
4. **改三处指向**（漏一处 = 新线没有闸）：
   - `.github/workflows/lts-api-guard.yml` 的 `branches:` → `LTS-v3.1`
   - `scripts/check-lts-api-surface.py` 的 `--against` 默认值 → `docs/api-surface/v3.1`
   - `qaqh-ringing/src/v2/types.rs` 的 `COMMAND_CONTRACT_REVISION` / `SESSION_CONTRACT_REVISION`
     bump（当前 `ringing-commands-7` / `ringing-session-2`）
5. 给 `LTS-v3.1` **新建**一份分支保护（§7 的命令换分支名）；旧线 `LTS-v3.0` 保留到 EOL。
6. 切 `v3.1.0` tag → `release.yml` 自动发 daemon zip + 契约 zip；前端 pin 新契约并对齐 hash
   （ADR-0006 落地前这步仍是人肉，也正是它要消灭的东西）。
