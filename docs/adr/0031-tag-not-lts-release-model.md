# ADR-0031 tag ≠ LTS：发版与冻结线解耦（规则 15/21 的松绑修订）

状态: `已采纳` · 决策日期 2026-11-19 · 决策人 仓库所有者（明确指示："有的时候我不想发LTS，
也没说必须每个tag都是LTS。所以有的设计是要松绑的，不然反而成宪法禁止的"）· 实施: 本批同落

## 背景（每条均已核实，附出处）

1. **宪法原文把 refreeze 绑死在 LTS 线上**：`AGENTS.md:26` 规则 15 原文——
   "`[contract-refreeze]`(wire 实变…**禁入已冻结线，只能开新 LTS 线并 bump revision**)"；
   `AGENTS.md:32` 规则 21 原文——"**本仓库为LTS仓**，遵循用户空间不可变"。两句合起来，
   任何 wire 实变的唯一合规路径 = 开 LTS 线。
2. **现实早已脱钩（git 实测）**：`v4.0.0`（2026-11-15）/ `v4.0.1`（2026-11-17）/
   `v4.0.2`（2026-11-18）三个 tag **全部从 main 直接切**，v4 无 LTS 分支；v4 时代的冻结
   闸门（`surface-policy.yml` / `check-surface-change-policy.py` 等）已随 `da33cbae` 剥离
   （ADR-0027 §背景 5）；v4.0 金标准在 main 就地机器重切是 ADR-0027 立的先例（裁定原话
   "用户空间不可变只在 LTS 仓库，当前无 LTS 发版计划"，`0027:43`）。
3. **字面规则在所有者意图下自相矛盾**：按规则 15 字面，v4.0.1 / v4.0.2 两次把 refreeze
   吸收进补丁位都属"违规"，只能被定性为 ADR-0027 的"已知不严格"欠账（`0027:48-61`）；
   而唯一合规路径（开 LTS 线）恰是所有者明确不想要的。字面没有给"不开 LTS 的正常发版"
   留任何合法位置——这正是所有者指出的"反而成宪法禁止的"。
4. **revision 三戳的语义对象不含服务面**（一手核实）：`SESSION_CONTRACT_REVISION` 的定义
   注释（`crates/qaqh-ringing/src/v2/types.rs:88-90`）："管理事件与读模型的**破坏性变化**；
   open 与 bootstrap 均携带，客户端必须严格校验"；`COMMAND_CONTRACT_REVISION`（`types.rs:86`）
   管命令目录（ADR-0028 更正记录实测：`-8` 删拒绝码、`-9` 加 goal 命令）；`wire_version`
   是 RingingV2 协议版本（`v2/mod.rs:25`）。服务方法新增（shell.*）与 additive DTO/能力位
   （`RingingV2Capabilities.shell_stream`）不触碰这三个对象的语义。ADR-0028 更正记录最后一行
   把"服务面要不要另设 revision 位"留给所有者，本 ADR 一并收口。

## 决策

1. **词汇正名（全仓适用）**：
   - **发版（tag）** = 推 `v*` tag → `release.yml` 产出三平台 daemon + 契约 zip。它是
     **版本化交付，不携带冻结承诺**；从 main 打 tag 是合法发版路径。
   - **LTS 线** = 独立分支承诺（冻结锁 + 守卫 + §7 登记纪律 + EOL 管理）。它是所有者的
     **显式可选动作**，不是 refreeze 的自动后果。当前仅 LTS-v3.0 / LTS-v3.1 存在；
     v4 是否开 LTS 线由所有者另行显式决定。
2. **规则 15 后果修订（main 线，v4+）**：`[contract-refreeze]` 的后果从"只能开新 LTS 线"
   修订为 **"禁入已冻结线 + 进新版本位"**：minor bump + 金标准机器重切（规则 16 机器写
   不变）+ exporter check 留证 + tag。三分类判据与 commit 标记本身不变。
3. **补丁位纪律保留**：refreeze 进补丁位（x.y.z 的 z）仍是**登记过的偏离**（ADR-0027
   教训保留），默认不做，要做必须在发版 ADR 里显式登记理由。松绑松的是"必须开 LTS 线"，
   不松"版本位语义"与"机器重切"。
4. **revision 三戳裁定（销掉两笔遗留张力）**：三戳只在**命令目录、事件/读模型词表、
   wire 协议**实变时 bump；服务方法新增、additive DTO、能力位扩展**不强制 bump**，兼容性
   由能力位机制（ADR-0028 §D3 约束 5 先例：`shell_stream` 显式声明）与 additive serde
   语义承载。由此：ADR-0027"三戳未 bump"从欠账改判为正常形态；ADR-0028 更正记录的
   "服务面 revision 位"张力裁定为**不设**（新增一个只为 additive 增量服务的戳 = 零信息量
   仪式，宪法 5）。
5. **LTS 线内部纪律一字不动**：LTS-v3.0 / LTS-v3.1 分支的冻结锁、守卫、§7 登记、热修
   纪律原样（两份 maintenance 文档的适用对象本来就是各自冻结线）。§8 换锁手册保留，仅在
   所有者显式开新 LTS 线时适用。
6. **历史不追溯**：v3.x 与 v4.0.x 的既有 tag、金标准、ADR 登记不回改；ADR-0027 只加
   状态注记指向本文（ADR 不可变纪律）。

## 被否选项

1. **保持字面**（refreeze 强制开 LTS 线）：把所有者明确不要的 LTS 变成 wire 演进的强制
   前提，非 LTS 发版 = 违宪——正是被指示推翻的死结。
2. **完全松绑**（refreeze 可随意进补丁位、可不机器重切）：ADR-0027 两次偏离已实证这是
   欠账形态；版本位语义（补丁 = 纯修复）与机器重切（规则 16）都保留。
3. **给服务面新设 revision 位**：ADR-0028 更正记录已论证服务方法不碰命令信封；永远为
   additive 增量 bump 的戳没有信息量（宪法 5 不写死钩子）。兼容性由能力位 + additive
   serde 承载。

## 后果

- main 线（v4+）的合规发版模型：`v*` tag = 交付；refreeze ⇒ minor bump + 机器重切 +
  check 留证；无 LTS 承诺，LTS 另议。
- AGENTS.md 规则 15/21 同步修订（本批）；引用旧字面的历史 ADR（0023/0027/0028）不回改，
  ADR-0027 加状态注记。
- **对 v4.1 评估的修正**：v4.1.0 是 main 线的普通版本位跳升，**不是 LTS 切线**。上轮
  评估的前置三件事中，"revision bump 方案裁定"由决策 4 收口，剩余前置只有：推送 main
  与既有 tag（本地领先 origin 1 commit）、入库在途 ADR-0029 批次（含金标准重切，exporter
  check 已实测 exit 0）、写 v4.1 发版 ADR（含本批 refreeze 清单登记）。
- 闸门冲击：无。exporter check 判据不变；被剥离的 LTS 闸门不复活。

## 落地清单

- [x] 本 ADR
- [x] `AGENTS.md` 规则 15/21 修订（含判据脚本已剥离的如实注记）
- [x] `docs/lts-v3.0-maintenance.md` 顶部范围注记 + §8 指针
- [x] `docs/lts-v3.1-maintenance.md` 顶部范围注记
- [x] ADR-0027 顶部状态更新注记
- [x] `docs/adr/README.md` 索引登记
