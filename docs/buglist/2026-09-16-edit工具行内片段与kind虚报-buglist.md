# buglist（2026-09-16）— edit 工具：`replace` 的整行语义没同步到模型可见的契约（行内片段 → 静默丢内容 / 误导性 NO_MATCH），且 schema 仍在宣传已删除的三个 kind

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 本条目**未单开 report**（证据自足并逐条内联于下）。
> 发现来源：**模型自报告** —— 本机 harness 在 `docs/buglist/2026-09-16-read_image连发触发400-buglist.md`
> 的 L15（超长表格行）上连做两次 `edit` 全是 `NO_MATCH`，而 `old` 逐字符就在那一行里；改用 `sed`
> 绕过后登记（机主：**「后续我通知团队优化 edit」**）。
>
> **共同根因**：`replace` 从 `a92626d` 起是**整行窗口**语义（`old` 必须是一行/多行的完整内容），
> 但**模型可见的两处文字仍按「片段可用」宣传**：工具描述里的 `Kinds: … insert_after/insert_before/
> replace_inline …` 与 `Use shortest unique old/anchor`。`a92626d`（2026-09-08，owner 拍板 kind 6→3）
> 删实现时只改了行为，**没改描述**——所以 06/07/08 是同一个「契约没同步」的三个出口。

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-16-06 | `fixed @61b39d0`（**静默丢内容**，优先级最高） | 行内片段 `old` 占所在行 ≥ ~85% 时被 Tier3 采纳，替换区间是**整个命中行**，`new` 把整行顶掉 ⇒ 该行未被 `old` 覆盖的前后缀**无提示删除**；返回仍是 `1/1 hunks applied … score 0.98` |
| BUG-2026-09-16-07 | `fixed @61b39d0` | 行内片段 `old` 达不到 0.85 时判 `NO_MATCH`，诊断文案是「best score 0.32 is below threshold 0.85 — closest location is probably wrong; re-check 'old' against the file」——**而 `old` 逐字符存在于候选行里**（候选 diff 的 `-` 行就是 `old` 原文）。模型据此判定「自己记错了内容」，真因却是「片段 vs 整行」 |
| BUG-2026-09-16-08 | `fixed @61b39d0` | 工具描述与 `hunks` schema 仍列 `insert_after` / `insert_before` / `replace_inline` 三个 kind，实现已在 `a92626d` 删除 ⇒ 任何按描述发起的调用恒 `PARSE_ERROR: unknown hunk kind '…' (expected replace / prepend_file / append_file)`。`edit/mod.rs:10-11` 的模块文档「能力面」清单、`transaction.rs:341` 的 `INVALID_REGEX → replace_inline` 提示映射、`tests.rs:565-579` 的 R19–R25 需求注释，同为残留 |

## 事实与证据

| # | 事实 | 位置 |
|---|---|---|
| 1 | `old` 先按 `\n` 切成 `pattern_lines`（**片段 ⇒ 1 行 pattern**），再与文件**整行窗口**比较 | `crates/qaqh-workspace/src/edit/matching.rs:6-12` |
| 2 | Tier1 是**逐行全等**（`fl[s+k] == pat[k]`）⇒ 片段永不命中 | `matching.rs:53-61` |
| 3 | Tier3 评分是**字符级** `TextDiff::from_chars().ratio()`（1 行片段 vs 1–3 行窗口）⇒ 「像」被当成「是」 | `matching.rs:96-103`、`matching.rs:211-279` |
| 4 | 采纳阈值 0.85 / 边际 0.10 | `edit/mod.rs:32-34` |
| 5 | 命中后的替换区间 = **整个命中窗口**（`char_starts[start_line]..char_starts[start_line+win_lines]`），不是 `old` 在行内的位置 | `resolve.rs:26-48`（`replace_range`）、`resolve.rs:92-107` |
| 6 | 失败诊断只有三种口径（差阈值 / 差边际 / 完全不像）——**没有「片段 vs 整行」这一种** | `matching.rs:190-204` |
| 7 | 三个 kind 只剩描述，解析器只认 3 个 | `handler.rs:344`、`handler.rs:356`（描述）vs `hunk.rs:54-100`（`match kind`）、`hunk.rs:97-99`（unknown kind 错误） |
| 8 | 收敛是**有意的**，只是没同步文字：`a92626d`「删 insert_after/insert_before/replace_inline 三 kind——replace 带 context 表达插入（old=锚行, new=锚行+新内容）；regex 替换走 bash/python」 | `git show a92626d`（2026-09-08） |
| 9 | 42 个 `#[test]` 里**没有一个**覆盖「片段 old」；R19–R25（replace_inline 的 sed `s///` 需求）随实现一起删除，只剩注释 | `tests.rs:565-579`（注释，无测试体） |

## 复现（本机实测；dry_run + 临时文件，未碰仓库）

### 07：片段逐字符存在却 NO_MATCH

`/tmp/edit-probe.md`：

```
# probe

| BUG-2026-09-16-XX | `fixed`（工作区，待提交） | some really long trailing text to make this line long |
| other row | x | y |
```

`edit` hunk0 = `replace`，`old = "`fixed`（工作区，待提交） |"`（**L3 的真子串**），`dry_run=true`：

```
hunk0 replace: NO_MATCH — best score 0.32 is below threshold 0.85 — closest location is probably
wrong; re-check 'old' against the file (see candidates below)
  candidate #1 L3-L3 score 0.32 tier3
  - `fixed`（工作区，待提交） |
  + | BUG-2026-09-16-XX | `fixed`（工作区，待提交） | some really long trailing text … |
```

会话里同一现象：`2026-09-16-read_image连发触发400-buglist.md` L15（长表格行）连失败两次。

> **示例行 ID 为占位（2026-09-17 卫生复核改）**：探针文件里的示例行原照抄真实登记号
> `BUG-2026-09-16-01`，与 `2026-09-16-read_image连发触发400-buglist.md` 的真实条目**撞号**——
> 任何 `^\| BUG-` 的 grep 都会把这段代码块误当成登记行。现改为占位号 `BUG-2026-09-16-XX`
> （**与原号等长，均 18 字符**，故行的长度、`old` 片段与记录里的 score/候选 diff 全部不变；
> 示例演示的是「`old` 是长表格行的行内真子串却 NO_MATCH」，与 ID 取值无关）。
> 示例行里的状态文案 `fixed（工作区，待提交）` 是**探针文件的伪造内容**、非登记状态，
> 保留原样以免改动复现记录（`rg '工作区，待提交'` 仍会命中这 4 行代码块，属预期）。

### 06：片段够「像」时整行被顶掉

`/tmp/edit-probe6.md`：

```
改前   prefix: aaa…a          （'a'×200，整行 208 字符）
改后   aaa…b                  （'a'×199 + 'b'，整行 200 字符）   ← "prefix: " 被静默删除
```

调用：`replace`，`old` 是 200 字符片段（占行 208 字符中的 200），文件另含两行互不相似的 80 字符
行。返回：

```
[OK] edit /tmp/edit-probe6.md
  1/1 hunks applied (new_hash b991b04f)
  hunk0 replace: L1 tier3 score 0.98 similarity 0.98
```

**没有 warning、没有提示**，前缀就这么没了。对照（同一片段、同一行）：

| 文件 | 第二候选 | 边际 | 结果 |
|---|---|---|---|
| 单行（`/tmp/edit-probe3.md`） | 0.97 | 0.01 | `NO_MATCH`（被拦） |
| 长行 + 1 行 32 字符（`/tmp/edit-probe4.md`） | 0.90 | 0.08 | `NO_MATCH`（被拦） |
| 长行 + 2 行 80 字符（`/tmp/edit-probe5.md`） | 未打印 | 边际 ≥ 0.10（推断） | **被采纳 → 整行顶掉** |

> 03/04 两行的「第二候选/边际」是失败诊断实测打印的；05 被采纳，诊断不打印候选，故只知边际 ≥ 0.10。

即当前**唯一**的护栏是「相邻窗口恰好不那么像」这件偶然的事，而不是「`old` 是否对齐行边界」。

### 08：三个 kind 恒 PARSE_ERROR

```
edit hunks=[{"kind":"insert_after","anchor":"…","new":"…"}]
→ edit: hunks[0]: unknown hunk kind 'insert_after' (expected replace / prepend_file / append_file)

edit hunks=[{"kind":"replace_inline","anchor":"…","old":"x","new":"z"}]
→ edit: hunks[0]: unknown hunk kind 'replace_inline' (expected replace / prepend_file / append_file)
```

## 影响

- **06**：无提示的数据丢失，且刚好落在「长行里改一小处」这种最需要行内编辑的场景（表格行、单行
  配置、长 URL/JSON、压缩过的代码）。`dry_run` 同样只回一行 ok 摘要，不翻开 `patch` 看不出前后缀没了。
- **07**：`Use shortest unique old` 把模型指到这条路上，拿回的却是「你可能记错了内容」；可靠做法只剩
  「先 read 整行、再整行 `replace`」——本会话其余两次 `edit`（`markdown.rs` / buglist 状态行）正是
  这么过的，能过纯属 `old` 恰好取了整行。
- **08**：描述与实现不一致是**模型可见的假契约**——照描述写就是必错，白烧一整轮。

## 修复方向（建议，未实施）

1. **先修文字（成本最低、收益最大）**：`handler.rs:344/356` 删掉三个 kind，把 `Use shortest unique
   old/anchor` 改成明确的整行指引（「`old` 必须是完整的行；行内改动 = `old` 取整行、`new` 给改后的
   整行」），`mod.rs:10-11` 的能力面清单同步；`hunk.rs:97-99` 的 unknown-kind 错误补一句「kind 已
   收敛为 replace / prepend_file / append_file」。
2. **给 06 加护栏**：Tier3 采纳前，若 `pattern_lines(old)` 的（唯一）非空行是某窗口行的**真子串**
   且未对齐行边界 ⇒ 不采纳，返回针对性的 `NO_MATCH` 诊断（「`old` 是行内片段，`replace` 是整行语义」）。
   判据要用「是否为整行及其前缀/后缀对齐」而不是 `ratio`——0.98 的 `ratio` 掩盖了 8 个字符的差异。
3. **或（若产品上仍需要行内编辑）把 `replace_inline` 实现回来**：按 `tests.rs:565-579` 的 R19–R25
   补齐 sed `s///` 语义（regex + 捕获组、`INVALID_REGEX`、窗口内 `replace_all`、模糊锚消歧）与测试；
   **不要**停在「描述里有、实现里没有」。
4. 无论走哪条：给「片段 old」补回归测试——06 与 07 各一条（06 用 `probe5` 的三行版做素材），先红后绿。

## 未做 / 待办

- 本条目**只登记**（机主：由团队优化 `edit`），未改任何代码。
- 06 的边界曲线**未穷举**：只验证了上表三档，没有扫「片段/整行长度比 × 邻行相似度」的完整边界；
  修 06 时建议直接把这条曲线补成测试素材。
- 复现用的 `/tmp/edit-probe*.md` 是临时文件、未进仓库；本文已内联全部输入与输出，可离线复原。
