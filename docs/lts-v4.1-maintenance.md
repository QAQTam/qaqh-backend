# LTS v4.1 维护发版说明书

> 适用分支:`LTS-v4.1` · 切线自 main @ `bb28211b`(2026-11-19,ADR-0032) · 线版本:`4.1.0`(分支上)
> 金标准:`docs/api-surface/v4.1/`(机器写;`--rebaseline` / `--pin-frozen` 属 `scripts/check-lts-api-surface.py`)
> 契约代际:`ringing-commands-10` / `ringing-session-3` / `wire_version 2`(沿 main 现值,ADR-0031 决策 4)
> 决策台账:`docs/adr/`——本文写**怎么做**,`docs/adr/` 写**为什么这样定、以及哪些方案被否**。
> 上一代锁的历史登记在 `docs/lts-v3.0-maintenance.md` / `docs/lts-v3.1-maintenance.md`(只读)。

## 0. 分支模型

| 分支 | 角色 | API 面(RingingV2 + SDK 契约) |
|---|---|---|
| `main` | 开发快车兼发版线(tag = 版本化交付,不携带冻结承诺,ADR-0031) | 允许演进;`contract-refreeze` 进新版本位 |
| `LTS-v4.1` | 用户空间依赖的稳定面 | **冻结**:§1 纪律 |

不变量:**`main` ⊇ `LTS-v4.1`**(宪规 19)。

## 1. API 面冻结纪律(发版前必读)

**冻结的定义**——LTS 分支上,以下内容禁止变更行为、字段、语义:

1. **HTTP 端点集合**:`/ringing/v2/*` 全部路由 + `/health` `/activity` `/control/v1/*`
   —— 由两张数据表登记并驱动 `build_router`(`qaqh_ringing::RINGING_V2_HTTP_ROUTES`
   与 `axum_impl::CONTROL_PLANE_ROUTES`)。加路由的唯一途径是往表里写一行;
   守卫 lint 禁止 `axum_server/**` 出现字面量 `.route("…")`/`.nest(`/`.merge(`。
   冻结粒度含 HTTP 方法(本线金标准内 `snapshots/routes.txt`,机器重切,勿手改)。
2. **信封与版本闸门**:`RingingV2CommandEnvelope` 字段、契约代际取值
   (`ringing-commands-10` / `ringing-session-3` 在本 LTS 生命周期内保持原值不动;
   仅当命令目录/事件读模型词表/wire 协议**实变**时才 bump,见 ADR-0031 决策 4)
3. **类型化方法表**:`TYPED_METHODS` 中每个方法的名称、params/result 形状、MethodKind、错误语义
4. **命令与事件面**:`RingingCommand`、`RingingV2Event` 各变体的字段与判别方式
5. **SDK 契约哈希**:偏离必须先在 §7 登记后才允许提交;additive 且不动契约代际的
   偏离可记在当前锁内,其余一律开新 LTS 线(v4.2)。

**允许的改动**:bug 修复、内部重构(不触及上述面的)、新增仅在内部使用的代码路径、测试。

## 2. 标准补丁发版流程(LTS-v4.1)

```bash
# 0. 确认在 LTS 分支且与 origin 同步(禁 git checkout,用 switch/restore)
git switch LTS-v4.1 && git pull

# 1. 补丁修复提交后,同步版本号(Cargo.toml / package.json;Cargo.lock 随任意 cargo 命令同步)
#    本线无 version.txt / sync-version.ps1(v4.0 从新时已拆,ADR-0032 §被否选项)

# 2. 本地守卫与契约验证
python3 scripts/check-lts-api-surface.py
cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.1

# 3. 打 tag 发布(当前未开远程分支保护,直推即可;日后开保护则走 PR)
git tag v4.1.<n> && git push origin LTS-v4.1 v4.1.<n>
#    → release.yml:三平台 --locked release 构建 → 打包 → 发 GitHub Release
```

产物(自动生成,勿手传):三平台 daemon zip/tar + `qaqh-sdk-contract-v4.1.<n>.zip`。

## 3. 紧急热修

热修 tag 必须走 `v4.1.<n>` 序号,禁止 `latest`/浮动 tag。若热修动了导出物 → commit 必须带
`[contract-refreeze]`,而那**不允许进 LTS**:说明它不是热修,是换锁,按 ADR-0031 走新版本位或开 v4.2。

## 4. CI 矩阵速查

| 工作流 | 触发 | 作用 |
|---|---|---|
| `lts-api-guard.yml` | push / PR → `LTS-v4.1` | 金标准逐字节比对 + 路由表/门面快照 + 锁身份(硬闸) |
| `surface-policy.yml` | push([main, LTS-v4.1]) / PR | 改动三分类;金标准比对 main report-only、LTS 硬拦 |
| `sdk-contract.yml` | push([main, LTS-v4.1]) / PR / 手动 | Rust 测试三件套 + 契约确定性 + 严格 TS 消费者 |
| `release.yml` | 推 `v*` tag / 手动 dispatch | 三平台 `--locked` release 构建、打包、发 Release |

外部依赖钉子:`qaqtam/mutil-ai` @ `b37ca25f23350a8c6cb78bc69fda023f2cb809aa`(随 main;LTS 生命周期内禁止升级)。

## 5. 与 v3 流程的偏差(所有者 2026-11-19 指示,如实登记)

- **远程分支保护未开启**(require PR / enforce_admins / required status checks 均未配,
  指示原话"锁远程分支就别做了"):纪律由守卫脚本 + 提交者自觉承载;直推合法。
  日后要开保护,按 `.github/CODEOWNERS` 头注的清单在 GitHub Settings 配置。
- **revision 三戳不随 additive/refreeze 自动 bump**(ADR-0031 决策 4):仅命令目录、
  事件/读模型词表、wire 协议实变时 bump。
- **tag ≠ LTS**(ADR-0031):`v*` tag 是版本化交付;本线的冻结承诺来自分支纪律,不来自 tag。

## 6. 发版 checklist

- [ ] 触碰 §1 冻结面的提交都已在 §7 登记(`check-frozen-ledger.py` 绿 = 机器形式)
- [ ] Cargo.toml / package.json / Cargo.lock 三处版本号同步
- [ ] `python3 scripts/check-lts-api-surface.py` 绿
- [ ] `cargo run -p qaqh-contract-export -- check --against docs/api-surface/v4.1` 绿
- [ ] tag 已推,Release 页面出现且标记 Latest,三平台资产 + 契约 zip 齐全
- [ ] 前端仓库把依赖 pin 到新 tag

## 7. 锁内已批准的契约修订登记

(空账本;格式:`### 7.<n> <一句话>`,写清变更、兼容性判定(additive/破坏性)、
是否 bump 契约代际、为何机器闸门放行。)
