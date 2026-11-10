# ADR-0007 换锁治理：CODEOWNERS + refreeze 标签 + 守卫自保护

状态: `已采纳（决策 1/2/4/5 已落地；决策 3 落了一半；LTS 分支保护已开启，两处故意缺口见手册 §7）` · 决策日期 2026-11-10

## 背景（已核对）

- `docs/lts-v3.0-maintenance.md` §1 写明金标准**只允许在两种情况下再生成**：
  ① 新 LTS 线切出；② LTS 上明确批准的契约修订 —— 并明确指出此时"它已不是冻结而是换锁，
  需 bump revision 并重切 tag"。
- `.github/` 下**无 `CODEOWNERS`**（实测 `Test-Path .github/CODEOWNERS` = False），
  仅三个 workflow：`lts-api-guard.yml`、`sdk-contract.yml`、`release.yml`。
- 触发面不对称：`lts-api-guard.yml:3-7` **只在 `LTS-v3.0`** 的 push / PR→LTS 跑；
  `sdk-contract.yml:3-7` 跑 PR / main / LTS-v3.0。

## 问题：闸门本身不受闸门保护

先澄清一件**已经成立**的好性质：手改 `docs/api-surface/**` 里的产物**会**被判红 ——
守卫在 `check-lts-api-surface.py:137-143` 现场重新导出并与金标准逐字节比对（`:76-98`），
人改的字节与代码生成的字节不一致即失败。所以"偷偷改快照让它变绿"这条路已经堵住了。

真正没堵的是另外三条：

1. **改松守卫本身**：PR 里动 `scripts/check-lts-api-surface.py`（缩小正则、跳过某个目录、
   把 `manifest.json` 加入豁免字段）同样能让 LTS 变绿。闸门代码不在任何评审强制路径上。
2. **"换锁"与"回退实现"在机器上不可区分**：一个合法的 refreeze PR（重生成快照）与一个违规的
   "顺手改快照" PR，**在 diff 形态上一模一样** —— 都是 `docs/api-surface/**` 变化。目前只有
   §1 那句散文能区分二者，而散文不是 required check。
3. **PR 目标分支选错 = 守卫根本不跑**：把本应进 `LTS-v3.0` 的改动提到 `main`，
   `lts-api-guard.yml` 因 trigger 不匹配而静默不执行（这与 ADR-0003 的 fail-open 是同一类形状）。
4. **金标准比对在非 Linux checkout 上结构性不可用**（实测，2026-11-10）：
   `.gitattributes` 是 `* text=auto`，于是 `docs/api-surface/**` 在 Windows checkout 里是 **CRLF**，
   而导出器写的是 **LF**。`check-lts-api-surface.py:96` 用 `read_bytes()` 比对 ⇒ 现场实测
   **599 个文件里 598 个"差异"**，全部只是行尾符（CRLF→LF 归一后 598/599 全等，唯一真差异是
   `manifest.json` 的两个 volatile 字段）。守卫因此**实际上只能在 CI（ubuntu-latest）跑**，
   手册里"本地预检金标准"的说法是不成立的。
   **受影响范围要划准**（实测）：只有 `compare_file_set` 的 `read_bytes()` 那一道（599 个产物）
   被行尾符打到；`compare_snapshot` 用 `read_text()`，Python 的 universal newlines 已把 `\r\n`
   归一 —— 把 `routes.txt`/`facade.txt` 临时改成 CRLF 再跑守卫自己的函数，**两种行尾都 PASS**。
   即这是一道**单点**缺陷，不是三道全废（写 ADR 时容易顺手夸大成"整个守卫在 Windows 不可用"）。
   **本条已于 2026-11-10 修好**（`52a0d0bb`，落在 `LTS-v3.0`，main 已 merge）：
   `.gitattributes` 加了 `docs/api-surface/** text eol=lf`，且 `compare_trees` 现在豁免
   `snapshots/` 并把 `manifest.json` 交给带 volatile 字段切除的语义比对。修好后
   `cargo run -p qaqh-contract-export -- check --against docs/api-surface/v3.0` 在 Windows
   checkout 上 **exit 0**（本机实测），所以它从此就是**正确的本地金标准预检命令**。
   撞车记录（ governance 价值的一条实证）：本 worker 在 `13e6dc1e` 里独立加了同一条
   `docs/api-surface/** text eol=lf`，与 `52a0d0bb` 在 merge 时正面冲突（且 git 会把
   `.gitattributes` 里的 `>>>>>>> origin/LTS-v3.0` 冲突标记**当成属性行解析**，报
   `not a valid attribute name`）。两人 20 分钟内各自修同一个洞、互不知情 ——
   这正是决策 1（CODEOWNERS）与决策 2/3（refreeze 标签 + 成文 ADR）要解决的形状，
   而不是抽象的官僚流程。合并时保留 `52a0d0bb` 的版本、删掉重复规则。
   撞车记录 2（2026-11-10 第二轮）：`origin/LTS-v3.0` 前进到 `cdaf6dbd` 之后，两个 worker
   在数十分钟内**各自**把 LTS merge 回 main 恢复超集不变量 —— 本地 `c8e06526`，
   对端 `b3f139a8`（它把这次 merge 标成 `[doc-only-refresh]`，而两侧改动都没碰金标准，
   分类闸对两者都输出`未触碰冻结金标准，规则不适用`）。合并无内容冲突
   （`presets.rs` 里那条新增只出现一次，实测计数 = 1）。
   留这条的真正原因不是追责，而是：**`main ⊇ LTS-v3.0` 目前没有机器闸** —— 它只写在
   规则 19 / 交接 §3，靠人记得 `git fetch`。把它做成闸门（在 `surface-policy` 里断言
   `git merge-base --is-ancestor origin/LTS-v3.0 HEAD`，仅对 target=main 生效）需要先回答
   "PR 分支落后于 LTS 时算谁的红"，属新增闸门 ⇒ 按 README §必须写 ADR 的条目另开一份，
   不在本轮顺手实现。

## 决策

1. **`CODEOWNERS` 覆盖闸门与其输入**，而不只覆盖业务代码：
   ```
   docs/api-surface/**            @<owner>
   scripts/check-lts-api-surface.py @<owner>
   scripts/check-sdk-contract.py  @<owner>
   tools/qaqh-contract-export/**  @<owner>
   crates/qaqh-ringing*/**        @<owner>
   crates/qaqh-client/**          @<owner>
   .github/workflows/**           @<owner>
   ```
   配 branch protection：上述路径的改动需 owner 批准（`Require review from Code Owners`）。
2. **`contract-refreeze` 标签 + 一个 required job**：凡改 `docs/api-surface/**` 的 PR 必须带该标签；
   带该标签的 PR **禁止合入 `LTS-v3.0`**（只允许合新 LTS 线，如 `LTS-v3.1`）。
   这把 §1 的散文变成一条机器不可绕过的路径，并首次让"换锁"与"回退"在流程上分岔。
   **但标签必须分两类，否则会把自己的闸门改进也堵死**（ADR-0003 已撞见实例：控制面入表会让
   `snapshots/routes.txt` 从 `* /health` 变成 `GET /health`，即"多钉住 4 个方法"）：
   - `contract-refreeze`：**wire 实变**（增删产物、类型字段变化）⇒ 用户可见面变了 ⇒ 禁入 LTS；
   - `guard-tightening`：**仅** `snapshots/*` 变化且导出物与 `contract_hash` 逐字节不变 ⇒
     冻结得更多而非改约 ⇒ 允许进 LTS（bug 级发版即可）；
   - `doc-only-refresh`（**由 ADR-0009 补的第三类**）：导出物字节变了但 `wire_view` 不变
     （典型是给 DTO 加了一段 doc 注释导致生成的 TS 多了 JSDoc，`contract_hash` 随之变化）
     ⇒ 允许进 LTS，**不**需要 bump revision。
   required job 判的是"变化的文件集合落在哪一类"，不是"有没有动 docs/api-surface"。
   分类判据本身从"字节"改为"wire 视图"的过程与被否方案，见 **ADR-0009**（触发它的是
   `ac54e508`+`4c3a26be` 那次零 wire 变化的注释改动被误判成换锁）。
3. **refreeze 必须成文**：带标签的 PR 附一份 ADR（模板见 `docs/adr/README.md`），
   内容至少含：新 `contract_revision` 值、被破坏的 capability 清单、迁移窗口、
   以及 ADR-0005 配对测试的结论。无 ADR 链接 ⇒ job 红。
4. **触发面补漏**：在 `main` 上也跑一次"导出 + 快照差异"，但**只报告不阻断**（main 允许演进，
   ADR-0001 §决策）；目的是让"本应进 LTS 的改动提错了 main"在 PR 页面就可见，而不是等 cherry-pick
   时才发现守卫从没跑过。
5. **金标准目录写权限收敛到导出器**：`main.rs:116-129` 已用 `generator_version` 做目录所有权检查
   （拒绝覆盖非本 exporter 拥有的目录/版本），保留并补一条：CI 中检测
   `docs/api-surface/**` 里出现**非导出物**的新文件即红（防止有人往金标准目录塞手写补丁文件）。

6. **让守卫在任意平台等价**（针对 §问题 4，二选一，倾向前者）：
   - `.gitattributes` 追加 `docs/api-surface/** text eol=lf`，把金标准钉成 LF，
     于是 `read_bytes()` 比对与 checkout 平台无关；导出器本身已是 LF，无需改动。
   - 或守卫读入时做行尾归一后再比对。**不选**"给 Rust `check` 加 `snapshots/` 豁免"作为主方案——
     那会让 `check --against` 有两种语义（自洽校验 / 对金标准校验），是新的含糊；
     金标准比对归 Python 守卫，`check` 保持"只比导出物对导出物"。
7. **手册的判定法改为可执行的两条**：本地跑 `scripts/check-sdk-contract.py --typescript`
   （确定性 + 严格 TS 消费者，与金标准无关，任何平台都成立）；金标准比对声明为
   **CI-only**，本地仅在 Linux/WSL 下建议跑 `scripts/check-lts-api-surface.py`。

## 被否选项

- **禁止任何人改金标准目录**：否。新 LTS 线切出时必须合法重切，硬禁会把正常发版卡死；
  需要的是**分岔路径**（标签 + ADR + 目标分支），不是禁令。
- **只加 CODEOWNERS 不加标签**：不够。见 §问题 2 —— 两种 PR 的 diff 形态相同，靠人读 diff
  区分"换锁"和"作弊"，正是本 ADR 要消除的判断成本。
- **在 main 上也阻断**：否，违反 ADR-0001 的快车前提（rule 3：不许拖慢 main）。
- **上 CI 模板/组织级 policy 引擎**（如 OPA 全量把关）：当前三条 workflow 的规模不值得，
  一个 required job 就够。

## 后果与代价

- 多一个 required check ⇒ 忘记打标签的合法 PR 会被拦，第一次会有点烦；但拦截方向是
  **"错合"而不是"漏改"**，符合 ADR-0001 §决策 3 的失败方向约定。
- CODEOWNERS 在小团队会退化为"自己批自己"。这是治理成本而非机制缺陷；即便如此，
  §2/§3 的标签+ADR 仍留下了**可追溯记录**，价值主要在事后而非事前。
- `main` 上的非阻断比对**不需要** release 导出：`surface-policy.yml` 里跑的
  `check --against docs/api-surface/v3.0`（debug 即可，`52a0d0bb` 之后它已豁免 `snapshots/`）
  加上秒级的 `--skip-export`（路由表 + lint + 门面）。原设想中的 `just surface-quick`
  配方由这两个标志替代，未另建 recipe。

## 落地清单

- [x] `.github/CODEOWNERS` 新建 —— **12 条路径**（比本文原列的 7 条多：补了路由登记处
      `axum_server/**` 与 `v2/mod.rs`、`.gitattributes`、`docs/adr/**`、手册本身）
- [~] `LTS-v3.0` 分支保护：**2026-11-10 已开启**（PR + 1 批准 + code-owner 要求 +
      required checks `contract`/`freeze` + strict + 禁 force-push/删除，回读确认）。
      两处**故意**没做满：① `change-policy` 未加进 required contexts，因为
      `surface-policy.yml` 目前只在 `main`，现在就加会让 LTS 每个 PR 永远停在 Expected；
      ② `enforce_admins=false` 保手册 §3 的热修直推口。详见手册 §7 的缺口表。
- [x] `contract-refreeze` / `guard-tightening` 两个 label 已建（描述改成了 ASCII：
      Windows 控制台下 CJK 有被写坏的风险，本轮实测后改写了一次）
- [x] `scripts/check-surface-change-policy.py` + `.github/workflows/surface-policy.yml`：
      按**改动落点**自动分类。实测 5 例全对 —— 只改 `snapshots/routes.txt` 的 0003 提交判
      `guard-tightening` 放行；引入整份金标准的 `b96f2b72` 无标记判红；同一改动声明
      `contract-refreeze` 且目标 `LTS-v3.0` 判红；目标换 `LTS-v3.1` 放行；无基线时
      明确提示"未经分类"而不是静默绿。
- [~] 决策 3（refreeze 必须附 ADR 链接）**只落了标签这一半**：脚本校验标记，暂不校验
      commit/PR 里是否真有 ADR 链接。有意留的半步：先让分类不可绕，门槛一次抬满只会逼人绕过。
- [x] 决策 4（main 上 report-only）已落地，但**不在 `lts-api-guard.yml` 里**：改在
      `surface-policy.yml` 用 `continue-on-error: target != 'LTS-v3.0'` 实现"main 只报告、
      LTS 硬拦"，免得两条 workflow 各跑一份 release 构建。
- [x] 手册 §4 增 `surface-policy.yml` 行与分类判据；新增 §7 写清需要 admin 的两项设置
- [x] `.gitattributes` 追加 `docs/api-surface/** text eol=lf`（由 `52a0d0bb` 落在 LTS，main 已 merge；
      本 worker 的重复规则在 merge 时删掉，保留一份）
- [x] 手册 §1 判定法改为可执行的两条：`check --against docs/api-surface/v3.0`（金标准，本地可用）
      + `check-sdk-contract.py --typescript --release`（确定性与严格 TS 消费者）
