# ADR-0013 旧史嫁接：QAQ-Harness 298 提交并入 qaqh-backend

状态: 已采纳 · 2026-11-12 · QAQTam（拍板）/ QAQ（执行）

## 背景

- 2026-08-22 rebuild 丢弃了旧 `.git`：新仓根提交 `d5ba44c7` 是孤儿（`parents:[]`，
  2026-08-22 02:30 +0800）。旧仓 `D:\QAQ-Harness`（GitHub `QAQTam/QAQ-Harness`）
  `main=7263f3c`，298 提交，2026-07-26 → 2026-08-21 21:28，无 tag/stash，工作区干净。
- 内容连续性已验证：旧 HEAD → 新根仅 7 文件差异（+32/−203）——版本
  `1.0.0-rc.8 → 0.0.0-alpha0`、workspace 移除 `apps/tui`、README/LICENSE/.gitignore/
  sync-version.ps1 重写；**源码树逐字节一致**。rebuild 是快照+元数据清理，不是分叉。
- 新仓全部 7 条远程分支均从该孤儿根下降 ⇒ 一次 graft 即可接通整棵树。

## 决策

1. 用 `git-filter-repo --commit-callback` 把新根的父亲设为字面量原始哈希
   `7263f3c0e63b9d674faffa577edd8a8425ac0201`。旧 298 哈希与 GPG 签名原样保留；
   新 1089 哈希重写；`main=1387`，单根 `135a5581`。新根重写后为 `776234ad`。
2. force-push 6 分支 + `v3.0.0` tag 到 `QAQTam/qaqh-backend`
   （`main 2b15cdc7→049bd9e0`，tag `ac459656→66e6cedb`）。基线取执行时最新 origin
   （含并发写入者刚推的 P0 attach 活锁修复 `2b15cdc7`，家规 19 先 fetch 比对）。
3. 旧 GitHub 仓 `QAQTam/QAQ-Harness` 归档（描述指向新仓）。
4. 哈希引用分两类处理：
   - 散文文档（ADR/交接/P0 报告/设计稿）与闸门脚本 **docstring** 里的引用：按权威
     map 重映射（66 处，0 歧义），叙述在新哈希下依然为真。
   - **机器-written 冻结产物不重映射**（家规 16）：`docs/api-surface/v3.0/manifest.json`
     的 `source_revision` 保持 `cdaf6dbd…`（导出确实发生在该提交，改写 provenance
     等于伪造历史记录）；describe 风格 `g<hash>` 同理。旧哈希经
     `docs/history/graft-commit-map.txt` 解析。
5. 本地工作仓只用 `update-ref` 对齐（禁 `git checkout`，家规 22），脏工作区
   63 文件逐字节保留；本地 main 落在 `db41f0ea`（`cc5f6bdf` 的重写），落后 origin
   的 2 提交由所有者就绪后正常 pull。

## 被否选项

- **B `git replace --graft`（零改写）**：本地 log/blame 连续，但 GitHub 网页无视
  replace refs ⇒ 网页历史依旧断裂，每个 clone 都要额外 fetch replace refs。
  属"看起来兼容"的拧巴态（宪法第 9 条），否。
- **C 归档分支（零改写）**：旧史作为 `archive/legacy-main` 并存，可浏览但与 main
  不相连，两段历史永久割裂，否。
- **A2 嫁接进旧仓 QAQ-Harness**：保留旧仓身份但须放弃活跃 CI/PR 仓（21 个 PR、
  LTS 闸门、并发工作流），迁移成本倒挂，否。
- **重映射冻结 manifest 的 provenance**：违反金标准机器写（家规 16）且伪造导出
  历史，否；用 map 文件解析代替。

## 后果

- **团队必须重新 clone**（或 fetch + reset）；已关闭 PR 页面的提交链接失效；
  旧哈希经 bundle/map 仍可解析。
- **filter-repo 陷阱（已实测）**：fast-export/import 往返会**剥离 gpgsig**，旧史
  239 个提交因此产生"影子链"副本（首个分叉 `c3c79cea`，GitHub 签名的 PR#1 merge；
  前 59 个无签名提交不变）。commit-map 里混有 old→影子 的**无效条目**
  （same=59/diff=1328）。权威 map = commit-map ∩ `rev-list(2b15cdc7)` = 1089 条，
  已存 `docs/history/graft-commit-map.txt`。main 实际祖先链是原始链（graft 父亲
  是字面量哈希），旧链签名完整；影子链仅存在于工作 mirror 的 archive ref，未推送。
- 新史提交若带 GPG 签名，重写必然使其失效——哈希改写的固有代价，非工具缺陷。
- 验证基线：6 分支 tip tree + tag tree 与 graft 前逐字节一致；旧史
  count=298 / 根 `135a5581` / HEAD tree `edbaba18` 不变；`main−legacy=1089`。
- 备份：`D:\Projects\qaqh-pre-graft-backup.bundle`（verify 通过，graft 前全 refs）；
  工作 mirror `D:\Projects\qaqh-rewrite-mirror`。两者为本地安全网，确认稳定后可删。
- 契约面零改动：`docs/api-surface/`、`snapshots/` 未触；
  `scripts/check-surface-change-policy.py` 仅 docstring 变化（py_compile 绿），
  不触发家规 15 分类。

## CI 余波（执行后核查，2026-11-12）

1. **一次性 classify 失效（graft 引入，已自愈）**：force-push 触发的 Surface policy
   run（43152954813）以 push 事件的 `before`（`2b15cdc7`，graft 前对象）为
   `SURFACE_BASE_SHA`，该对象在新史中不存在 ⇒ `git diff` exit 128。后续正常 push
   的 classify 步骤已通过（849020f2 run 实证）。**不为此加防御性 fallback**（宪法 5）：
   一次性历史事件，不是 recurring 失效模式。
2. **facade baseline 红 = 预存治理债（非 graft 引入，未掩盖）**：Surface policy
   自 `cc5f6bdf`（LTS 收口 merge，新史 `db41f0ea`，run 43039744069）起即红。证据链：锁登记于
   `4e0f7160`（tree `e0fc731c` = pinned，登记时正确）；收口 merge 带入 `d134d9a6`
   （stream line-delta estimates，**wire 实变**：`contract_hash`
   `c25b08f1… → 1a0db0e0…`）使 main 金标准树变为 `e626a3c5`，未走 refreeze；
   graft 前后该树哈希逐字节一致（`e626a3c5`），与 graft 无关。按 ADR-0011，
   在 main 上 `--pin-frozen` 改绿 = 偷换锁，禁止；正路是 ADR-0010 的 v3.1 换锁线
   （所有者待拍板：revision 取值与前端先行）。**本 ADR 只登记事实，不代替决策。**
3. **sbx-win hardening 的 `sddl_snapshot_roundtrip_restores_dacl` 本机失败 =
   预存环境性**：在 graft 前代码（849020f2 临时 worktree，共享 target）上 bisect，
   同一机器同一失败（恢复后 DACL 多 `AI` 控制旗，ACE 全同）——scratch 位于用户
   Temp，ACL 继承链机器相关。非本批在制品回归（该批 acl.rs 改动经逐行核对为纯
   Option 包装迁移）。同套件已有环境敏感先例（`escape_probe` 显式 ignored）。
   是否被 windows CI matrix 覆盖未验证；测试本身的去环境化设计留待所有者决定。
