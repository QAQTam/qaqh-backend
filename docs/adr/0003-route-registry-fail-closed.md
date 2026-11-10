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

## 落地清单

- [ ] `CONTROL_PLANE_ROUTES` 常量 + `build_router` 改为表驱动（缺 arm panic 语义与 v2 面一致）
- [ ] `routes.txt` 由两份表生成；删 `check-lts-api-surface.py` 的 `ROUTE_CHAINED`/`ROUTE_ARM`
- [ ] `validation.rs` 反向覆盖闸（含 `uncovered` 显式声明语法）
- [ ] `.nest()` 隐藏控制面路由的回归用例（守卫必须红）
- [ ] `coverage.rs` 的 `path` 列改为对表全等校验，禁复合字符串
- [ ] 手册 §1 冻结面条目补注控制面归属
