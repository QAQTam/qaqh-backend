# LTS v3.0 维护发版说明书

> 适用分支:`LTS-v3.0` · 起点版本:`v3.0.0`(tag 于 main @ `7fe35d96` 切出,与 LTS 分支同源)
> 配套 CI:`.github/workflows/release.yml`(发版)、`.github/workflows/sdk-contract.yml`(契约校验)

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
2. **信封与版本闸门**:`RingingV2CommandEnvelope` 字段、`contract_revision` 的取值
   (`ringing-commands-7` / `ringing-session-2` 在整个 LTS 生命周期内**保持原值不动**)
3. **类型化方法表**:`TYPED_METHODS`(`qaqh-runtime/src/ringing/service_methods*.rs`)中每个方法的
   名称、params/result 类型形状、MethodKind、错误语义
4. **命令与事件面**:`RingingCommand`、`RingingV2Event` 各变体的字段与判别方式
5. **SDK 契约哈希**:`qaqh-contract-export` 的 `contract_hash` 相对 `v3.0.0` release 内契约 zip **不得变化**
   (相对 v3.0.0 的偏离必须先在 §7 登记后才允许提交;additive 且不动 `contract_revision` 的偏离可记在
   当前锁内,其余一律开新 LTS 线)

**允许的改动**:bug 修复、内部重构(不触及上述面的)、新增仅在内部使用的代码路径、测试。

**改动是否踩线的判定法**——先分清各闸门实际比什么(别把纪律当成 CI):

| 闸门 | 实际比对对象 | 能抓到什么 |
|---|---|---|
| `lts-api-guard.yml` → `scripts/check-lts-api-surface.py` | HEAD 导出物 vs **仓内** `docs/api-surface/v3.0/` golden(逐字节,`manifest.json` 的 `source_revision`/`source_dirty` 除外)+ 路由表 + facade 清单 | 实现改了 wire 但没同步 golden(或反之) |
| `sdk-contract.yml:48-49` | `export --out $T` 后 `check --against $T`,即**同一份** temp | 导出不确定/不可复现 |
| `scripts/check-sdk-contract.py --typescript --release` | 由导出物自算 `contract_hash` 校验 manifest 自洽 + 严格 TS 消费者编译 | 契约自相矛盾、前端可见字段被删(表现为 `error TS2353`) |

**没有任何 CI 步骤把 `contract_hash` 与 `v3.0.0` release 资产比对**——§1.5 那条是**人工纪律**,唯一执法者
是 §6 checklist 与 §7 登记。LTS 上不允许用"顺手改夹具"的方式让它变绿(夹具只能修编译性破损,不得改描述
被测行为的断言)。

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
| `release.yml` | 推 `v*` tag(发布)/ 手动 dispatch(干跑) | 双平台 `--locked` release 构建、打包、发 Release |

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

- [ ] `git log v3.0.<n-1>..LTS-v3.0` 里触碰 §1 冻结面的提交都已在 §7 登记(未登记 = 本次发版阻断)
- [ ] `contract_hash` 与上一版 release 的契约 zip 一致;不一致时 §7 必须有对应条目,且偏离是 **additive + 未 bump `contract_revision`**
- [ ] §7 登记条目里写清了「机器闸门为什么不会拦」(golden 同提交即绿),别让下个维护者以为 CI 兜住了锁
- [ ] `version.txt` 已改为本次补丁戳,Cargo.toml / package.json / Cargo.lock / qaqh-backend.lock.json 全部同步
- [ ] sdk-contract 在 LTS-v3.0 绿
- [ ] (标准流程)dispatch 干跑双平台绿
- [ ] tag 已推,Release 页面出现且标记 Latest,资产齐全
- [ ] 前端仓库把依赖 pin 到新 tag(契约 zip 里的 `QAQH_SOURCE_REVISION` 与 tag 提交一致)

## 7. 锁内已批准的契约修订登记

> 「换锁」的判据不是补丁号进位(3.0.0→3.0.1 只是版本号),而是这条锁的参照物有没有被改动后仍留在锁内。
> 换锁 = bump `contract_revision` + 重切 tag,那属于开新 LTS 线(§1.2/§42);登记在案的 additive 偏离则
> 留在当前锁内,以 golden 目录为锁内新基线。**换锁 ≠ 破坏性改动**:删字段/改语义是破坏性,加可选字段不是。

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
