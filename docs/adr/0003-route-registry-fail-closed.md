# ADR-0003 冻结守卫必须 fail-closed：路由 registry 单一真相源

状态: `已采纳（未实施）` · 决策日期 2026-11-10 · **修订 r2（2026-11-10 实施 ADR-0008 第 1 步时核实）**

> ⚠️ **本文初版有实质性错误**：它断言"本仓没有路由 registry、coverage 是手抄表、虚报只能靠人评审发现"。
> 实际核对后确认 **registry 已存在、router 由它驱动、虚报早已被闸住**，而且比我提议的方案更硬
> （缺 arm 直接启动 panic）。以下为重写后的版本；真正剩下的缺口比初版小得多，也更精确。

## 背景（已核对的代码事实）

**registry 已存在**：`crates/qaqh-ringing/src/v2/mod.rs:36`

```rust
/// Ringing v2 规范 HTTP 路由表(B02 漂移闸的契约侧真源)。
/// - daemon `build_router` 遍历本表注册 handler:表里有而 handler match 缺 arm
///   = 启动即 panic(响亮失败,不允许静默 404);
/// - contract exporter 校验 coverage 声明的路由 ⊆ 本表,双侧不得再漂移。
/// `/health`、`/activity`、`/control/v1/*` 是控制面观测/停机端点,不属于
/// Ringing 契约面,不在本表。
pub const RINGING_V2_HTTP_ROUTES: &[(&str, &str)] = &[ /* 16 条 */ ];
```

三道既有机制，全部**正确且 fail-closed**：

| 机制 | 位置 | 性质 |
|---|---|---|
| router 由表驱动注册 | `axum_server/axum_impl/mod.rs:113-122`（`for (method, path) in RINGING_V2_HTTP_ROUTES`） | 表里有、handler match 缺 arm ⇒ **启动 panic** |
| 虚报闸 | `tools/qaqh-contract-export/src/validation.rs:110-135` | coverage 声明的路由 ∉ 表 ⇒ 导出失败 |
| 反向语义 | 同文件注释 `:114-116` | handler arm 的路径字符串以表为准，不得自造 |

B02 事故（`coverage.rs:54-55` 注释记录的那次"coverage 虚报了从未存在的 `/sessions/{id}/driver/*`"）
**正是这套机制的由来**，不是它失效的证据。初版把它当成"手抄表无人校验的证据"，读反了。

## 真正剩下的缺口（三条，都很具体）

### 缺口 1：控制面 4 条端点不在任何 registry 里

`axum_impl/mod.rs:117-121` 用字面量注册：

```rust
.route("/health", get(health))
.route("/activity", get(activity))
.route("/control/v1/stop", post(handle_stop))
.route("/control/v1/stop-if-idle", post(handle_stop_if_idle));
```

而 `docs/lts-v3.0-maintenance.md` §1 冻结面第 1 条**明确把它们列为冻结对象**
（"`/ringing/v2/*` 全部路由 **+ `/health` `/activity` `/control/v1/*`**"）。
表注释说它们"不属于 Ringing 契约面"——这话对 **Ringing 契约**成立，对 **LTS 冻结面**不成立。

结果：这 4 条唯一的安全网是 `scripts/check-lts-api-surface.py:31-32` 那两个正则
（`ROUTE_CHAINED` / `ROUTE_ARM`，从源码文本抓 `.route("…", get|post|…)`）。
`validation.rs:120` 的虚报闸只处理 `path.starts_with("/ringing/v2/")`，**控制面直接跳过**。
⇒ 控制面若改用 `.nest()`、变量拼 path、或 `.merge()`，**漏报是静默的，守卫照样绿**。
这才是 fail-open 的确切位置：不在 v2 面（那里 panic-on-missing-arm 已兜住），在控制面。

**而且这条洞已经被实测到了**（2026-11-10 现场核对，非推演）：`ROUTE_CHAINED` 只捕获 path、
不捕获 method，于是它对这 4 条产出的是通配形式。当前快照里控制面 4 行的真实内容是

```
* /activity    * /control/v1/stop    * /control/v1/stop-if-idle    * /health
```

即 **LTS 冻结面今天并没有钉住这 4 个端点的 HTTP 方法** —— 把 `/health` 从 `GET` 改成 `POST`，
`routes.txt` 仍是 `* /health`，守卫**全绿**（v2 那 16 条不受影响：它们走 `ROUTE_ARM`，方法是从
match arm 的 `("GET", "…")` 字面量里取到的，所以是有方法的）。
对比结果实测为：`union(表) − regex = {GET /activity, GET /health, POST /control/v1/stop,
POST /control/v1/stop-if-idle}`，`regex − union = 同 4 条的 * 形式`，`EXACTLY EQUAL = False`。

这条比 `.nest()` 假设更硬：它是一个**已经存在于金标准里的松匹配**，而且修法就是把控制面入表。

### 缺口 2：`snapshots/routes.txt` 是同一真相的第三份描述

守卫把正则结果写进 `routes.txt` 与快照比对（`check-lts-api-surface.py:58-67,112-116`），
但 v2 面的真相**已经在 `RINGING_V2_HTTP_ROUTES` 这个编译期常量里**，导出器也早已链接它
（`validation.rs:122`）。用正则从源码文本重推一遍，等于再造一份可能静默变窄的描述。

### 缺口 3：反向覆盖未强制（registry 有、capability 没记）

`validation.rs` 只查 `coverage ⊆ 表`。反方向（表里每条 route 至少被一个 capability 记录、
或被显式声明为不覆盖）**无检查**。⇒ 新增一条 v2 路由而忘记写进 `coverage.rs`，
导出成功、守卫绿，而前端从 `coverage.json` 里根本看不见它。

附带：`coverage.rs` 的 `path` 列仍是手打字符串，且允许 `"GET + POST"`、`"A + B"` 这类复合形态
（`validation.rs:116-120` 为此做了归一与跳过）。手抄本身不是错，但归一规则与跳过条件让这条闸
比它看起来更松。

## 决策

1. **控制面入表**：在 daemon 侧新增 `CONTROL_PLANE_ROUTES: &[(&str, &str)]`，
   `build_router` 的 4 条字面量改为由它驱动注册（与 v2 面同构：缺 arm ⇒ 启动 panic）。
   命名与归属写清楚：**它属于 LTS 冻结面，不属于 Ringing 契约面**，因此只进快照闸，不进 v2 虚报闸。
2. **`routes.txt` 改由两份表生成**（`RINGING_V2_HTTP_ROUTES` + `CONTROL_PLANE_ROUTES`），
   **删除 `ROUTE_CHAINED` / `ROUTE_ARM` 两个正则**。快照覆盖面从此等于编译期常量，不再等于正则完备性。
   落地方式：v2 那 16 条**不再从源码猜** —— 导出器已经链接 `qaqh-ringing`，加一个
   `routes-dump` 子命令把 `RINGING_V2_HTTP_ROUTES` 原样打到 stdout，守卫读它即可；
   这条路不新增导出物，因此**不触发 refreeze**（`snapshots/` 本就被比对逻辑豁免，见 ADR-0007 §问题 4）。
2b. **把"收集型"正则换成"禁止型"lint —— 这才是 fail-closed 的确切形状。**
   正则的问题不是"抓不全"，而是**它失败时表现为成功**（少抓一条 = 快照少一行 = 依旧绿）。
   反转之后：在 `crates/qaqh-daemon/src/axum_server/**` 里，除表驱动循环内部那一处
   `router.route(path, …)` 之外，**出现任何 `.route(` 字面量即红**。
   这样 `.nest()`/`.merge()`/变量拼接/新写法统统不会被"漏抓"，而是**不认识就报错**。
   同一条 lint 还顺手管住 `nest`/`merge`：`Router::nest(` 与 `Router::merge(` 一并禁止，
   除非显式登记在豁免表里（带理由）。
2c. **行为侧再兜一层**：daemon 加一个测试，对两份表的每条 `(method, path)` 用
   `tower::ServiceExt::oneshot` 打一次请求，断言**不返回 404**（路径参数用合成值填），
   并断言一个未登记路径返回 404。这把"表与真实 router 等价"从结构断言升级为行为断言，
   与 ADR-0005 的配对测试同源，但成本是一次进程内调用、无网络无构建。
3. **反向覆盖闸**：`validation.rs` 增加一条 —— 表里每条 route 必须被 ≥1 个 capability 引用，
   或在 `coverage.rs` 里显式标 `uncovered`（带理由）。虚报与漏报从此对称。
4. **回归用例锁死这个性质**：故意在 daemon 里用 `.nest()` 挂一条控制面路由 ⇒ 守卫必须红。
   v2 面不需要这条（缺 arm 已经 panic），靶心是控制面。
5. 保留 `coverage.rs` 的 capability 语义（scope / lease / types / fixtures 这些**表里没有的信息**），
   但 `path` 列改为从表取（或由表校验为全等字符串，禁止复合形态），消除缺口 3 的成因。

## 被否选项

- **保留正则并加模式**：否。理由不变且仍成立：正则对 Rust 源文本的完备性无法证明，
  而它每次失败都表现为"CI 绿"。
- **让控制面进 `RINGING_V2_HTTP_ROUTES`**：否。那会把它拖进 v2 虚报闸与 `coverage.json` 的
  Ringing 语义（scope/lease），而它是观测/停机面，不属 Ringing 契约。单表 + 只进快照闸才对。
- **初版提议的"新建 capability registry 取代 coverage.rs"**：否，**已被现实超越** ——
  registry 已经存在，重建它是重复劳动。本修订把工程量收敛到"控制面补表 + 删正则 + 反向闸"。

## 后果与代价

- 工程量比初版小一个数量级：一张 4 行的表 + 删两段正则 + 一条反向检查。
- 表与 handler match 的同步靠 panic 保证，**不需要测试覆盖也能响亮失败**；这是既有设计，
  本 ADR 只是把控制面纳入同一形状。
- `docs/lts-v3.0-maintenance.md` §1 冻结面第 1 条的措辞应补一句"控制面同样由表驱动"，
  避免后来者再得出"控制面无主"的结论。

## 落地清单（2026-11-10 实施，见 main 上的对应提交）

- [x] `CONTROL_PLANE_ROUTES` 常量（`axum_server/axum_impl/mod.rs`）+ `build_router` 改为表驱动，
      缺 arm ⇒ `panic!("control-plane route without a registered handler: …")`，与 v2 面同构
- [x] `routes.txt` 由两张表生成；**删除 `ROUTE_CHAINED` / `ROUTE_ARM` 两个正则**，
      换成严格表解析：const 缺失 / 未闭合 / 行形状不对 ⇒ 直接 `SystemExit`（fail-closed）
- [x] **禁止型 lint**：`axum_server/**`（去注释后）出现 `.route("…")` 字面量、`.nest(`、`.merge(` 即红。
      实测反证：临时插一行 `router.clone().route("/ghost", get(health))` ⇒ 守卫报出
      `axum_impl/mod.rs:155` 并 exit 1
- [x] **行为测试** `v2_route_table_equals_the_real_router`（`axum_server/route_table_tests.rs`）：
      20 条表项逐个 `oneshot`，断言不落 fallback（fallback 签名 = 404 + body `"not found"`）；
      6 条幽灵路径断言**必须**落 fallback；路径参数未登记合成值 ⇒ 断言失败。
      探针用 **view scope 设备凭证**，因为两个 stop 端点先过 `require_scope(Admin)`
      （`control.rs:40-42` / `:55-57`）—— 否则 `TEST_SERVICE` 这个跨测试 `OnceLock`
      会被真关掉，同二进制里后续 daemon 测试全废。实测 1 passed / 0.12s
- [x] `validation.rs` **反向覆盖闸** + 取消对复合路径的跳过：`expand_routes()` 展开
      `"GET + POST"` / `"/a/b + /events"` 这类声明，正向（声明 ⊆ 表，含复合）与
      反向（表 ⊆ 声明）都查；方法数与路径数不匹配 ⇒ 判为含糊声明报错。
      实测反证：往表里加一条 `("GET","/ringing/v2/__ghost__")` ⇒
      `RINGING_V2_HTTP_ROUTES declares GET /ringing/v2/__ghost__ that no coverage capability documents`
- [x] `UNCOVERED_ROUTES` 目前是 `[0]` —— 16 条 v2 路由**全部**已有能力记录，无需豁免项
- [x] 重切快照：`--rebaseline`（机器写，人不手改）。实测 diff **恰好 4 行**
      （`* /health` → `GET /health` 等），`facade.txt` 无变化，
      导出物与 `contract_hash` 逐字节不变（`check --against docs/api-surface/v3.0` exit 0）
- [x] `.nest()` / `.merge()` 分支：**手工反证已做**（临时插 `router.clone().nest("/ghost", Router::new())`
      ⇒ 守卫报 `axum_impl/mod.rs:185` 并 exit 1，撤回后转绿）。
      顺带修掉 lint 自身的一个误导：去注释时原先是**删行**，导致报出的行号比真实位置小 30 行；
      现在改成置空行保号 —— 一个报错误行号的 lint 本身就是新的误导源。
      仍**没有**常驻自动化用例（不为此建一套 python 测试脚手架，规则 1）；
      下一次有人动这个脚本时应重复这次反证。

## 与决策的偏离（记录以免被误读为设计漂移）

决策 2 里写的 `routes-dump` 子命令**没有实现**，因为实施时发现更省事且更强的做法：
守卫直接严格解析两张 `pub const` **数据表**（文本解析，但只解析固定形状的表，
且"解析不出即报错"）。好处是①守卫不再需要 cargo 构建就能查路由（`--skip-export`），
②零新增导出物、零跨 crate 依赖（导出器要读 daemon 的 const 就得反向依赖 daemon）。
表若被人用非字面量形式改写（变量、宏展开），严格解析会直接失败，不会静默漏抓 ——
这正是当初否决"保留正则"的理由，所以偏离是朝同一个方向收紧的。
