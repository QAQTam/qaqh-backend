# ADR 台账

本目录是**后端契约与分支模型的决策记录**。`Cargo.toml` 中 `undocumented_unsafe_blocks = "deny"`
的注释早已指向 `docs/adr`，本目录即其落点。

## 什么情况必须写 ADR

1. 任何触及 `docs/lts-v3.0-maintenance.md` §1 冻结面的提案 —— **被否决也要写**，否则同一提案会反复重提；
2. 任何机器闸门的**新增或退役**（CI job、守卫脚本、导出工件）；
3. 任何"为了兼容再加一层转换"的念头被接受或被拒绝时（项目宪法第 9 条要求留痕）。

## 状态词表

| 状态 | 含义 |
|---|---|
| `已采纳` | 决策定稿 **且代码已落地** |
| `已采纳（未实施）` | 决策定稿，代码未动。**禁止把此状态读成"已完成"** |
| `提议中` | 等待拍板 |
| `已拒绝` | 明确不做的方案，防止反复重提 |
| `已取代 by ADR-xxxx` | 旧决策作废，保留以追溯 |

## 模板

```
# ADR-NNNN <标题>
状态: <词表之一> · 决策日期 · 决策人
## 背景        —— 只写已被代码验证的事实，附 file:line
## 决策        —— 祈使句，一句一个可验收的动作
## 被否选项    —— 选项 + 为什么不做（这是 ADR 的主要价值）
## 后果        —— 代价、失败模式、对本仓库现有闸门的冲击
## 落地清单    —— 未实施时逐条可勾
```

## 索引

| 编号 | 标题 | 状态 |
|---|---|---|
| [0001](0001-kernel-userspace-branch-model.md) | kernel/user-space 分支模型与 LTS API 冻结 | 已采纳 |
| [0002](0002-contract-source-of-truth-no-openapi.md) | 契约真相源 = Rust DTO；拒绝 OpenAPI 承载 RingingV2 | 已采纳（含 `已拒绝` 方案） |
| [0003](0003-route-registry-fail-closed.md) | 冻结守卫必须 fail-closed：路由 registry 单一真相源 | **已落地于 main**（表驱动 + 删正则 + 禁止型 lint + 双向覆盖闸 + 行为测试）· r2 修订：registry 本就存在 |
| [0004](0004-wire-compat-classes.md) | wire 兼容方向是声明数据，不是隐含 serde 行为 | **已采纳（部分实施）** · §待决**已决**：出站引入 `unknown` fallback（入站维持 reject）· 落地清单**前两条 fail-open 已落 `main`**（契约盲区入导出物 + `.ok()` 改显式失败） |
| [0005](0005-n-minus-one-pairing-tests.md) | 兼容性由 N-1 配对测试证明，不由字节相等证明 | 已采纳（未实施） |
| [0006](0006-contract-as-versioned-package.md) | 契约分发为版本化包 + provenance | 已采纳（未实施） |
| [0007](0007-refreeze-governance.md) | 换锁治理：CODEOWNERS + refreeze/tightening 标签 + 守卫自保护 | **已落地于 main** + **LTS 分支保护已开**（缺口表见手册 §7） |
| [0008](0008-neutral-ir-per-language-projection.md) | 中立 ContractIR + 每语言投影，退役 ts→cs 解析链 | **第 1 步 + 第 2 步的量尺已落地**（`schema-preview` + `ir-diff`）· 替换投影与删解析机未实施 · §第 2 步（量）记着实测：ts-rs 无结构化 IR ⇒ 原计划"双跑逐字节 diff"改为属性级对账 |
| [0009](0009-wire-view-vs-artifact-bytes.md) | 冻结粒度区分 wire 语义与产物字节：分类从两类改三类 | **已落地于 main**（`wire_view` + `doc-only-refresh`）· 由真实误判触发 |
| [0010](0010-cut-lts-v3.1-line.md) | 换锁：v3.1 线的待拍板取值与攒着的 refreeze 清单 | `提议中` · §待决 2 **已决：暂不切线**（所有者 2026-11-10）· revision 取值与前端先行仍待拍板 |
| [0011](0011-frozen-lock-identity.md) | 冻结线的锁必须有登记身份：main 上「改金标准让它变绿」这条路从此被封死 | **已采纳（agent 落地于 main，待 owner 复核）** · 反证已做 |
| [0012](0012-exec-pwsh-uutils-preamble.md) | exec 的 pwsh 壳集成 uutils coreutils：探测式 preamble 注入 + 最小别名手术 | **已采纳（代码已落地）** · vendor 分发与引导下载延后（另行 ADR） |
| [0013](0013-legacy-history-graft.md) | 旧史嫁接：QAQ-Harness 298 提交并入 qaqh-backend（1387 连续历史） | 已采纳 · 已执行并 force-push · commit-map 与 gpgsig 陷阱见正文 |
| [0014](0014-subagent-as-session.md) | 子代理=会话：单权威收敛与跨会话消息（collector/双身份机构退役，wire 不变） | 已采纳（未实施）· 实施排序受 widget-directive 在途改动约束 |

## 阅读顺序

0001 定义"什么不许变"；0002 定义"以什么为真相源、拒绝什么外框"；
0003/0004 修现有闸门的**失效模式**（fail-open、语义未声明）；
0005 把证明方式从"字节相等"升级为"行为配对"；0006/0007 管分发与治理；
0008 是 0003/0004/0005 的承载体 —— registry 与 compat 声明最终都长在 IR 上；
0010 是 0007 那条"换锁只能走新线"的**账本**：main 上攒了几笔 refreeze、切线时要不要 bump
代际、由谁拍板，都记在那里。
0011 补 0007 剩下的最后一个缺口：分类闸只看"改没改金标准",看不见"改了契约却没动金标准",
于是 main 上那条 report-only 的红可以被"重切一下"抹掉 —— 锁的身份从此必须登记。
