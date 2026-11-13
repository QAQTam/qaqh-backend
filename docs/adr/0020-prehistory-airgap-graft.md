# ADR-0020 前传与黑暗 71 天：空气-gap graft 接通 DSX→DeepX→QAQ-Harness

状态: 已采纳 · 2026-11-13 · QAQTam（拍板「接头」+ 空气提交设计）/ QAQ（执行）

## 背景

QAQ-Harness 半周年（2026-05-14 DSX 开工 → 2026-11-14）。所有者要证明这六个月是
**同一个项目的连续演化**，而非散落的重启，并确认继承关系 > 50%（求职/留痕用）。

前传史实（详见 `docs/history/prehistory-continuity-S5.md`，下称 S5 报告）：

- **DSX**（2026-05-14 开工）：`D:/dsx` 重建链 May14→16→18。母仓 `E:/coding/dsx`
  失落（E 盘不存在），GitHub `QAQTam/dsx` 已 404。`D:/tools/dsx` 是 May18 孤儿快照，
  其 `.git` gitfile 指向 `E:/coding/dsx/.git/worktrees/hopeful-einstein-937e5e`——
  worktree 链接证明同仓族。
- **DeepX-Desktop**（June2 前后）：品牌 DSX→DeepX 改名窗口 05-18→06-02（crate 仍
  dsx-*）。原仓 `D:/project/DeepX-Desktop` 失落（D:/project 整个没了）。June2 态由
  `C:/Users/QAQTam/Downloads/xs/session.json` 工具结果 partial 重建（recon 仓）。
- **黑暗 71 天**（June2→0726）：原仓 git log/status 当时已 30s 超时，提交全失。
  0726 deepx「Initial commit」`135a5581` 是存世根。`xs/lib.rs`（74.5KB 单体
  deepx-msglp Loop，Round v5、pre-Ring）是黑暗期唯一存世中间修订残片。
- **0726→qaqh**：已由 ADR-0013 graft 接通（旧 QAQ-Harness 298 提交，根 `135a5581`）。

四段血缘裁决（S5 报告 §6）：A(May14→18) 真实同仓；B(May18→June2) 内容连续提交全失；
C(June2→0726) 内容连续提交全失；D(0726→qaqh) 真实已 graft。

## 继承关系度量（诚实分层，不灌水）

**「继承关系」= 血缘/设计延续，在架构与协议设计层面 > 50%，可验证：**

| 度量 | 数值 | 方向 |
|---|---|---|
| 协议语义存活 June2→0726 | **80%**（16/20 变体；Ui2Agent 5/6、Agent2Ui 11/14） | founding→deepx |
| ToolCall 字段存活 | 100%（id/name/action/args 原样） | founding→deepx |
| DebugSnapshot→Dashboard 字段逐字 | 87.5%（7/8） | founding→deepx |
| 核心子系统存活 June2→当前 | **100%**（8/8：agent loop/proto/session/config/gate/tools/types/message 均有 qaqh-* 后裔） | founding→当前 |
| 根同一 | **100%**（当前产品根提交即 0726 deepx Initial commit 同一对象） | 血缘 |
| 单调 superset 链 | Ui2Agent 6→11→20，死变体不复活 | 线性演化 |

跨三代示踪符号：`AgentState`、`dsml_compat_count`（DeepSeek 指纹，0726 后退役）、
`CANCEL` AtomicBool、`seed`、`UsageInfo`、`MessageStore`、`agent_protocol.rs` 文件名、
JSON-LP + `tag="type"` 线格式。活到当前 RingingV2 的 founding 符号：TurnStart/TurnEnd、
AskUser/AskQuestion、PermissionRequest、Dashboard、UsageInfo、SessionMeta、AgentState。

**诚实边界（必须同时声明）**：原始**代码行**层面的继承 **< 50%**。当前 39 crate 中
ringing/sandbox/ptc/mcp/lsp/acp/ffi/harmony-napi 等大量是六个月**新长出来的**——这是
生产力证明，不是减分。**不得声称「今天 50% 的代码是继承的」**；可声称「founding 架构与
协议设计 80% 存活、核心子系统 100% 延续、产品根提交即 deepx Initial commit」。

## 决策：空气-gap graft（所有者设计）

把前传链接到 deepx 根 `135a5581`，**在每个失落提交的边界插入空树「空气」提交**显式标注
断裂，而非假装逐字节连续：

```
b5e92c61 May14 DSX根 → a8ae908e → 509de87a → 6ce50eb1 May16(脱敏) → a4acff28 May18(脱敏)
  → 883ca4f4 空气B（失落改名窗口 May18→June2，空树）
  → 34e4587f June2快照重建 → cf94fc0b June2·S5报告
  → df787f55 空气C（黑暗71天 June2→0726，空树）
  → d3adddbb 0726 deepx「Initial commit」（原 135a5581，重写）
  → …1411… → 461734c9 main
```

**为什么空气提交是诚实的**：`空气C→0726` 的 diff 是「从零长出整棵树」⇒ 0726 提交
**仍语义上是 Initial commit**（对父全量新增）；`June2→空气C` 的 diff 是「全部删除」⇒
如实标记 71 天失落。这不是伪造连续，而是**显式标注断裂**。空气提交树 = 空树
`4b825dc642cb6eb9a060e54bf8d69288fbee4904`，消息写明「NOT a real empty state」。

**与 ADR-0013 的关系**：ADR-0013 标准是「接合点逐字节一致才接」——那是针对**真实连续**
的旧→新 graft。本前传接合点**无法逐字节验证**（71 天失落 + June2 是 partial 重建），
故不直接硬接，而以空气提交标注断裂。这是 ADR-0013 诚实性原则的**正确推广**：真实连续
→直接 graft；失落gap→空气-gap graft。两者都拒绝伪造 provenance。

## 被否选项

- **硬 graft（June2 直接作 0726 父亲）**：假装逐字节连续，伪造 provenance，违 ADR-0013
  诚实性，否。
- **`git replace --graft`（零改写）**：GitHub 网页无视 replace refs，网页历史依旧断裂，
  拧巴态（宪法 9），ADR-0013 已否，沿用。
- **纯 archive ref（零改写，不相连）**：证据可浏览但与 main 不相连，无法证明六月链
  visible，所有者明确要「接头」，否。（archive ref 仍保留作命名指针，见下。）
- **声称代码行继承 > 50%**：与事实不符（新 crate 大量新增），灌水会在求职被穿，否。
- **GitHub unblock-secret 让真密钥过**：把 DeepSeek/GitHub 旧密钥推进 GitHub 历史，即便
  已撤销也不专业（求职语境尤甚），否；改脱敏。

## 执行（filter-repo，照搬 ADR-0013 机器；含脱敏）

1. 备份：`D:/Projects/qaqh-pre-prehistory-backup.bundle`（--all，verify 通过）；
   bare mirror `qaqh-prehistory-mirror3`（最终脱敏版）。
2. 前传链用 `commit-tree` 造：DSX 链 fetch；空气B/C 空树；June2 两提交保留 recon
   原树（`8936ed5c`/`6d5242f0`，逐字节）与原消息，只换父亲。
3. **GitHub secret-scanning push protection 要求脱敏**：前传 `session_dump.jsonl`
   （15.3MB 会话档案）含 **3 个唯一 DeepSeek `sk-` 密钥（18 处，含 3 处 Bearer 头）
   + 1 个 GitHub OAuth `gho_` token（6 处）**。全谱扫描（GitHub 全变体/Anthropic/
   Google/Slack/Stripe/GitLab/AWS/JWT/npm）确认其余为 0。全部替换为 `[REDACTED-*]`
   （脱敏 blob `b567ce39`），档案其余逐字节保留。**D:/dsx 本地原件不动**，仅 graft
   版本脱敏。temp-index 重建 May16/May18 两提交（diff 仅 session_dump.jsonl，已验证），
   其上重造空气B/June2/空气C。
4. `git-filter-repo 2.47.0 --commit-callback`：根 `135a5581` 父亲设为字面量空气C
   `df787f55`（外部父亲机制同 ADR-0013 的 7263f3c）。`--replace-refs delete-no-add`
   不留 replace refs。4.38s。
5. **逐字节验证（全绿）**：
   - main tip 树 `45571936` 原==新；
   - 0726 新根 `d3adddbb` 树 `911d22c0` == 原 `135a5581` 树，父 = `df787f55`；
   - v3.0.0 树 `f4606881`、v3.1.0 树 `3d9f6b4b`、pin 树 `9ea9a388` 原==新；
   - 空气B/C 树 = 空树 `4b825dc6`；May16/May18 脱敏 diff 仅 session_dump.jsonl；
   - **权威 map = commit-map ∩ rev-list(main) = 1411/1411**（main 1420 祖先 − 9 保留
     前传 = 1411 全映射，无影子链）；pin/legacy 均为新 main 祖先。
6. 本地工作仓 `update-ref` 对齐（禁 checkout，家规 16）：main/LTS=`461734c9`，
   树相同故**工作区 0 脏文件**（ADR/history 文件除外，重新提交）；tag/archive 重指。

## 关键哈希 map（权威全表见 docs/history/prehistory-commit-map.txt）

| 对象 | 旧 | 新 |
|---|---|---|
| 0726 deepx 根 | 135a5581 | d3adddbb |
| 旧 QAQ-Harness 头 | 7263f3c0 | 3366a958 |
| main / LTS-v3.1 | 6ce3eb3b | 461734c9 |
| electron backend pin | 94e719db | 8303c5ed |
| tag v3.0.0 | 66e6cedb | 65280fb2 |
| tag v3.1.0 | 6b690dd2 | 0360f432 |
| 产品根（May14 DSX） | — | b5e92c61（保留） |
| 空气B（改名窗口） | — | 883ca4f4（保留） |
| 空气C（黑暗71天） | — | df787f55（保留） |

前传 9 提交原样保留（外部 graft 祖先，未重写）：b5e92c61/a8ae908e/509de87a/6ce50eb1/
a4acff28/883ca4f4/34e4587f/cf94fc0b/df787f55。其中 6ce50eb1(May16)/a4acff28(May18)
为脱敏重建（session_dump.jsonl 密钥替换）。

## 后果

- **全部 1411 提交重哈希**（含旧 QAQ-Harness 298 及其 GPG 签名再次失效）——比 ADR-0013
  更重：昨天 graft 保留旧 298，本次前传接的是旧 298 的根 `135a5581`，故整棵树重哈希。
  这是根前接 graft 的固有代价，非工具缺陷。备份 bundle 可回滚。
- **团队必须重新 clone**（或 fetch + reset）；已关闭 PR 页面提交链接失效；旧哈希经
  `prehistory-commit-map.txt` + `graft-commit-map.txt` 解析。
- **electron 重 pin**：`build/repositories.json` backend `94e719db → 8303c5ed`
  （pin 树逐字节一致 `9ea9a388`，仅哈希变）。electron 历史不改写，正常 push。已发布
  v3.1 artifacts 是冻结产物，不追溯改 pin；是否重发布由所有者定。
- **force-push** main + LTS-v3.1 + v3.0.0/v3.1.0 到 `QAQTam/qaqh-backend`。
- **前传档案脱敏**：session_dump.jsonl 的 3 DeepSeek + 1 GitHub OAuth 密钥在 graft
  版本中替换为 `[REDACTED-*]`；D:/dsx 本地原件保留完整档案。脱敏仅影响前传两提交树，
  backend 1411 提交内容零改动。
- **冻结 manifest provenance 不重映射**（家规 16，同 ADR-0013）：
  `docs/api-surface/v3.0/manifest.json` 的 `source_revision` 保持旧哈希，经 map 解析。
- **契约面零改动**：`docs/api-surface/`、`snapshots/` 未触；main tip 树逐字节一致 ⇒
  不触发家规 15 分类。
- CI 一次性 classify 失效（force-push 的 before SHA 在新史不存在）会自愈，同 ADR-0013
  §CI余波1，不加防御性 fallback（宪法 5）。
- archive ref 保留作命名指针：`refs/archive/legacy-main`=3366a958、
  `worktree-snapshot-20261108`=2bdf5e42；新增 `refs/archive/prehistory-dsx`=a4acff28、
  `prehistory-deepx-desktop`=cf94fc0b、`prehistory-msglp-monolith`=xs 残片归档（recon
  orphan 分支 msglp-monolith）。

## 失落坐标（不可恢复，仅留痕）

- `E:/coding/dsx/.git`（DSX 母仓，E 盘不存在）
- `D:/project/DeepX-Desktop`（June2 原仓，D:/project 没了）
- GitHub `QAQTam/dsx`（404）
- xs 兄弟模块 agent.rs/lifecycle.rs/dashboard.rs/logger.rs（仅 lib.rs 存世）
- 黑暗期 06-02→07-26 全部原始提交

## 留痕产物

- `docs/history/prehistory-continuity-S5.md`（S5 连续性度量全文）
- `docs/history/prehistory-commit-map.txt`（1411 old→new + 9 保留）
- `docs/history/prehistory-ref-map.txt`（分支/tag/archive 映射）
- 前传链内 `cf94fc0b` 提交自带 S5 报告（June2 重建态）
- recon 仓 `D:/Projects/deepx-desktop-recon`（June2 重建 + xs orphan 归位）