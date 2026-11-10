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

**允许的改动**:bug 修复、内部重构(不触及上述面的)、新增仅在内部使用的代码路径、测试。

**改动是否踩线的判定法**:本地两条都跑 ——
① `cargo run -p qaqh-contract-export -- check --against docs/api-surface/v3.0`(金标准比对。自 `52a0d0bb`
起 `compare_trees` 豁免 `snapshots/`、`manifest.json` 走切除 `source_revision`/`source_dirty` 的语义比对,
且 `.gitattributes` 已把金标准目录钉成 `eol=lf`,故 Windows 与 Linux 同样可用,本机实测 exit 0);
② `scripts/check-sdk-contract.py --typescript --release`(产物确定性 + 独立严格 TS 消费者)。
判定:金标准、契约哈希或严格 TS 消费者任一失败,即踩线;LTS 上不允许用"顺手改夹具"的方式让它变绿
(夹具只能修编译性破损,不得改描述被测行为的断言)。

**漂移事故参照**:model-config 重构删除 `SessionSettingsPatch.profile` 时,TS 严格消费者在 CI 抓到漂移
—— LTS 上这类漂移 = 发布阻断,必须回退实现而非改夹具。

**机器闸门**:`.github/workflows/lts-api-guard.yml`(push LTS / PR → LTS 触发)运行
`scripts/check-lts-api-surface.py`,把 HEAD 的契约导出物与 `docs/api-surface/v3.0/` 金标准逐字节比对
(manifest 中随提交变化的 `source_revision/source_dirty` 除外),并冻结 HTTP 路由表与 qaqh-client facade 清单。
CI 红 = 拒绝该改动;金标准只允许在两种情况下再生成:① 新 LTS 线(如 v3.1)切出时;
② LTS 上明确批准的契约修订(此时它已不是"冻结"而是"换锁",需 bump revision 并重切 tag)。
再生成命令:`QAQH_SOURCE_REVISION=$(git rev-parse HEAD) QAQH_SOURCE_DIRTY=0 cargo run -p qaqh-contract-export -- export --out <tmp> --release`,
随后把产物复制进金标准目录、连同 `snapshots/routes.txt`、`snapshots/facade.txt` 一并提交。

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

# 3. 推分支 → sdk-contract CI 必须绿
git push

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

流程同上,但允许跳过步骤 2/4,第 3 步 CI 绿了直接打 tag。热修 tag 仍必须走 `v3.0.x` 序号,禁止 `latest`/浮动 tag。

## 4. CI 矩阵速查

| 工作流 | 触发 | 作用 |
|---|---|---|
| `sdk-contract.yml` | PR / push(main、LTS-v3.0)/ 手动 | Rust 测试三件套 + 契约确定性 + 严格 TS 消费者 + 契约哈希锁定 |
| `lts-api-guard.yml` | push / PR → `LTS-v3.0` | 金标准逐字节比对 + 路由表/门面快照(硬闸,仅冻结线) |
| `surface-policy.yml` | PR / push(main、LTS-v3.0) | **改动分类闸**:`contract-refreeze` vs `guard-tightening` 判定 + 路由登记处 lint + 门面;金标准比对在 main 上 report-only、在 LTS 上硬拦 |
| `release.yml` | 推 `v*` tag(发布)/ 手动 dispatch(干跑) | 双平台 `--locked` release 构建、打包、发 Release |

`surface-policy.yml` 的判据是**改动落点**,不是人写的标签文案:只动 `snapshots/*` ⇒ 要求
`guard-tightening`;动了导出物(`manifest.json`/`types/`/`cs/`/`fixtures/`…)⇒ 要求
`contract-refreeze`,且**禁止目标为 `LTS-v3.0`**(换锁属新线,如 `LTS-v3.1`)。标记可写在
commit subject/body 的 `[guard-tightening]` 或 PR label(仓库已建好这两个 label)。
实测:引入整份金标准的 `b96f2b72` 被该闸判红;ADR-0003 那次只改 `routes.txt` 的提交
自动判为 `guard-tightening` 并放行。

外部依赖钉子:`qaqtam/mutil-ai` @ `4387f74d2eff05b76fbbc3fa4a41b1a2fdf52ad4`。
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

- [ ] `git log v3.0.<n-1>..LTS-v3.0` 里没有触碰 §1 冻结面的提交
- [ ] `contract_hash` 与上一版 release 的契约 zip 一致(或差异有成文说明且仅限 main 线)
- [ ] `version.txt` 已改为本次补丁戳,Cargo.toml / package.json / Cargo.lock / qaqh-backend.lock.json 全部同步
- [ ] sdk-contract 在 LTS-v3.0 绿
- [ ] (标准流程)dispatch 干跑双平台绿
- [ ] tag 已推,Release 页面出现且标记 Latest,资产齐全
- [ ] 前端仓库把依赖 pin 到新 tag(契约 zip 里的 `QAQH_SOURCE_REVISION` 与 tag 提交一致)

## 7. 仓库设置：LTS-v3.0 分支保护（2026-11-10 已开启）

经所有者授权，已用 `gh api PUT /repos/QAQTam/qaqh-backend/branches/LTS-v3.0/protection` 开启并回读确认：

| 项 | 值 | 说明 |
|---|---|---|
| Require a pull request before merging | ✅（1 个批准） | **直推冻结线这条路从此关闭** —— 本轮"两个 worker 各修同一个缺陷并 merge 撞车"的那个入口 |
| Require review from Code Owners | ✅ | 见下方"尚未生效的一半" |
| Require status checks | ✅ `contract`、`freeze` | context 名是**从 API 实测取的**（不是猜的）：`sdk-contract.yml` 的 job 名 `contract`、`lts-api-guard.yml` 的 job 名 `freeze` |
| Strict（分支必须先与 LTS 同步） | ✅ | 两个 worker 并发时防"基于陈旧分支合入" |
| Require conversation resolution | ✅ | — |
| Allow force pushes / deletions | ❌（禁用） | 冻结线不可改写历史 |
| **Do not enforce for admins** | ⚠️ **留口**（`enforce_admins=false`） | 热修手册 §3 依赖 admin 直推。要真锁死就把这项改 true，代价是热修必须走 PR |

### 两个**故意留着**的缺口（不要误以为全通了）

1. `change-policy`（即 `surface-policy.yml` 的 job）**没有**加进 required contexts ——
   因为 workflow 文件是从**被推的那个 ref** 读取的，而 `surface-policy.yml` 目前只在 `main` 上。
   现在就加，LTS 的每个 PR 会永远停在 "Expected"，等于把冻结线焊死。
   **等 `surface-policy.yml` + `.github/CODEOWNERS` 进了 LTS，再把这个 context 补上**：
   `gh api POST .../protection/required_status_checks/contexts --input '{"contexts":["change-policy"]}'`
2. 同理，`Require review from Code Owners` 已勾但 `.github/CODEOWNERS` 还不在 LTS ref 上 ⇒
   当前对 LTS 的 PR 是"无匹配 owner → 不触发 code-owner 要求"。第 1 条落地时这条自动一起生效。

`main` **未加保护**（快车，规则见 `docs/adr/0001` §决策）。
