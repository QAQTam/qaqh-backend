# ADR-0032 切 LTS-v4.1 线：新一期 LTS 开发的机器按需恢复

状态: `已采纳` · 决策日期 2026-11-19 · 决策人 仓库所有者（指示原话："开一个分支，LTS-v4.1，
准备开始进入新一期的LTS开发……那些金标准什么的按需拿回来，锁远程分支就别做了"）· 实施: 本批同落

## 背景（每条均已核实，附出处）

1. **owner 指示开线**（2026-11-19 夜）：`LTS-v4.1` 自 main 切出，进入新一期 LTS 开发；
   金标准等机器**按需**恢复；远程分支保护**不做**。
2. **机器在 v4.0 从新时被整体拆除**：`da33cbae`（"4.0 from-scratch: strip v3.x LTS gates"）
   删了 4 个守卫脚本、3 个 workflow、CODEOWNERS、`.gitattributes` 的 api-surface LF 钉子、
   `snapshots/frozen-locks.json` 与 v3.0/v3.1 金标准树（提交信息自列拆除清单）。
   拆除点即恢复真源：`da33cbae^`（`git restore --source` 取回）。
3. **代码剥离点只有注释与提示语**：`route_table_tests.rs` / `axum_impl/mod.rs` / `platform.rs`
   的改动均为注释级（route-count 断言现行口径是"同步更新本断言的预期值"），无需恢复。
4. **发版模型已由 ADR-0031 收口**：tag ≠ LTS；refreeze 进新版本位；三戳只在词表/协议实变时
   bump。本线 `ringing-commands-10` / `ringing-session-3` / `wire_version 2` 沿用不 bump
   （自 v4.0.0 起无命令/事件词表实变——shell.* 是服务方法，`created_at_ms` 是 additive DTO）。
5. **切线基线**：main `bb28211b`（含 ADR-0029 批次、created_at_ms 批次、ADR-0030/0031），
   `exporter check --against v4.0` 与 `cargo check`（8 crate）均已实测绿。

## 决策

1. **分支**：`LTS-v4.1` 自 main `bb28211b` 切出并推送 origin；`main ⊇ LTS-v4.1` 超集不变量
   沿宪规 19 维护。
2. **线版本**：分支上 workspace `4.0.2 → 4.1.0`（Cargo.toml / package.json；Cargo.lock 随
   构建同步）。**不随切线打 `v4.1.0` tag**——tag 待线就绪另发（推 tag 即触发 release.yml
   三平台发版）。
3. **机器按需恢复**（真源 `da33cbae^`，指向全部改 v4.1）：
   - `scripts/check-lts-api-surface.py`（默认 `--against docs/api-surface/v4.1`；失败文案指向
     `docs/lts-v4.1-maintenance.md`）
   - `scripts/check-frozen-ledger.py`（账本指向 `docs/lts-v4.1-maintenance.md` §7；自检样本
     随之改 v4.1）
   - `scripts/check-surface-change-policy.py`（`FROZEN_BRANCHES` 加 `LTS-v4.1`；refreeze 语义
     按 ADR-0031 措辞修正）
   - `scripts/check-sdk-contract.py`（无 v3 硬编码，原样取回）
   - `.github/workflows/{lts-api-guard,surface-policy,sdk-contract}.yml`（分支 LTS-v4.1；
     surface-policy 金标准比对 `--against v4.1`，main 保持 report-only 语义；sdk-contract
     保留 main 触发——它只做临时导出自比，main 安全）
   - `.github/CODEOWNERS`（原样取回；无分支保护时为评审指引）
   - `.gitattributes` 的 `docs/api-surface/** text eol=lf` 钉子（防 autocrlf 字节假阳性）
4. **金标准**：`docs/api-surface/v4.1/` 由 exporter 机器重切（规则 16）+ `--rebaseline`
   快照（routes/facade）+ `--pin-frozen` 锁身份登记（ADR-0011）。main 的 `v4.0/` 原地保留不动。
5. **线手册**：`docs/lts-v4.1-maintenance.md`（§7 为锁内偏离登记账本，
   `check-frozen-ledger.py` 盯它）。
6. **远程分支保护不开启**（所有者明确指示）：require PR / enforce_admins / required checks
   均不配；纪律由守卫脚本承载，直推合法。CODEOWNERS 头注保留配置清单以备日后启用。
7. **不恢复的非冻结机器**（按需的另一半）：`build.rs` 的 git-commit 解析、`version.txt`、
   `scripts/sync-version.ps1`、justfile sdk-verify、probes/ 探针——它们属发版工程而非
   冻结纪律，v4.0.x 两个 tag 已证明现发版流可用。

## 被否选项

1. **直接在 main 上做 4.1**（沿用 v4.0 的无分支模式）：所有者明确要分支承载新一期 LTS 开发，
   且冻结纪律需要独立的金标准树与锁身份（ADR-0011），main 单树承载两代金标准会互相污染。
2. **wholesale 恢复 v3 全部机器**（含 build.rs/version.txt/sync-version）：违反"按需"指示，
   把发版工程与冻结纪律混回来；v4 发版流已验证可用。
3. **切线即打 `v4.1.0` tag**：tag 触发 release.yml 三平台发版；线刚切、就绪与否未验，
   发版是独立的所有者动作。
4. **bump 契约代际**：ADR-0031 决策 4——无词表/协议实变不 bump；新设服务面戳已被否
   （ADR-0031 §被否选项 3）。

## 后果

- LTS-v4.1 上三 workflow 实跑（push 触发）：freeze/change-policy/surface-policy 首跑结果
  在 GitHub Actions 可核；本地已先跑守卫与自检留证。
- main 无守卫的现状维持（ADR-0031 后 main 唯一官方判据 = exporter check）；surface-policy
  在 main 上 report-only（金标准比对步骤 continue-on-error），主推 wire 变更时页面可见信号。
- 本线的 refreeze 若发生 → 进**新版本位**（v4.1.x 内为补丁位吸收=登记偏离；默认走 main 的
  minor 位）或开 v4.2；三戳按 ADR-0031 决策 4 判定。
- `main ⊇ LTS-v4.1`：切线提交（本批）尚未 ff-merge 回 main——按宪规 19 在下次 main 同步时
  吸收（分支上新增的金标准/机器/手册 main 迟早要有或显式裁定不吸收）。

## 落地清单

- [x] 分支 `LTS-v4.1` 切出并推送
- [x] 机器恢复 + v4.1 指向适配（脚本 ×4 / workflow ×3 / CODEOWNERS / .gitattributes）
- [x] 线版本 `4.0.2 → 4.1.0`（Cargo.toml / package.json）
- [x] 金标准 `docs/api-surface/v4.1/` 机器重切 + rebaseline + pin-frozen
- [x] 本地守卫验证（`check-lts-api-surface.py` / exporter `check --against v4.1` / ledger 自检）
- [x] 本 ADR + `docs/lts-v4.1-maintenance.md`
- [x] 推分支后核对 GitHub Actions 三 workflow 实跑结果（2026-11-19 实测全绿：LTS API guard
      2m42s / Surface policy 2m42s / Rust SDK contract 首跑红于内嵌 TS 消费者的 v3 `driver`
      探针，`d2107ec1` 适配 v4.1 字段后绿）
- [ ] `v4.1.0` tag 与首发：线就绪后由所有者另发
