<!-- CODEGRAPH_START -->
## CodeGraph

In repositories indexed by CodeGraph (a `.codegraph/` directory exists at the repo root), reach for it BEFORE grep/find or reading files when you need to understand or locate code:

- **MCP tool** (when available): `codegraph_explore` answers most code questions in one call — the relevant symbols' verbatim source plus the call paths between them, including dynamic-dispatch hops grep can't follow. Name a file or symbol in the query to read its current line-numbered source. If it's listed but deferred, load it by name via tool search.
- **Shell** (always works): `codegraph explore "<symbol names or question>"` prints the same output.

If there is no `.codegraph/` directory, skip CodeGraph entirely — indexing is the user's decision.
<!-- CODEGRAPH_END -->

# 项目宪法（v2 · 高质量发展修宪版 · 2026-11-26 颁布）

> **修宪声明**：自 `v4.6` 起，本项目正式废除旧版“为了快速上线开快车、跳过必要守卫”的早期迭代口径，全面转向**高质量发展、长期架构纯洁与零技术债演进**。旧宪法中仍有效的工程底线（零兼容层、零防御性补丁、ADR 留痕、资源保护、安全 Git、实名提交、上帝文件止血等）已全部吸收升级并重新编纂为以下五章 17 条最高准则；所有贡献者与 AI Agent 必须无条件遵守。

---

## 第一章：工程哲学与高质量发展总纲

1. **高质量发展最高原则**：以架构纯洁性、内存/并发确定性、可机器验证的正确性为第一目标。宁可先冻结设计再落刀，严禁为了“先跑起来”制造半吊子架构、临时胶水或明知必返工的过渡方案。
2. **零兼容架构与一次性清剿**：严禁保留为了兼容旧代际而堆砌的转换层、适配壳、`legacy_*` 分支、空壳 re-export 或 `#[serde(alias)]`。遇到架构冲突时，以更优的长期架构为唯一准绳；破坏性重构允许且鼓励，但必须在完成前置设计后**同提交/同窗口物理删除旧代码**，绝不拖泥带水。
3. **零防御性补丁（Fail-Closed）**：功能不能工作即代码错误，必须根治而非靠补丁让它“看起来能跑”。严禁吞错（如用 `.ok()` 静默忽略协议/状态错误）、伪随机降级、静默回退默认值等防御性烂代码；一切校验、握手、解析默认 **Fail-Closed**。
4. **前沿架构对标**：开工前鼓励分析 GitHub 同类优秀项目（如 Zed / WezTerm / iroh / Codex 等）的架构取舍，并将可借鉴点与本仓约束对比同步给用户。

---

## 第二章：协议最高准则与反腐迁移铁律（RingingV3 修正案）

5. **Ringing 协议最高准则与初心（服务最先进前端与最终用户体验）**：QAQHarness 的 Ringing 协议（正由 V2 演进至纯血 Rust Native 的 **RingingV3**，见 `docs/adr/0042-ringing-v3-rust-native-ipc-and-quic.md`）视作全仓最高准则，任何模块（含 AgentLoop、存储层、工具层）必须为其让路；**严禁出现 V2-over-V3 / V3-to-V2 转换层**，此类设计属于懒政，一律打回。同时必须恪守 2026-07-31 Ringing 创世初心——**协议是为前端开发与用户最终展示效果服务的，绝不是用来折磨前端开发的**：
   - **顺应最先进前端范式**：协议必须主动释放现代原生前端的硬件红利（零拷贝 `Bytes` 图像直传 GPU、原始 PTY 字节直喂终端模拟器、结构化原生组件替代 HTML iframe），绝不用旧 WebView/旧壳层绑架协议演进；
   - **开箱即渲染（View-Ready）**：后端直接交付权威快照（原生支持尾部窗口与向上分页）、强类型 `ToolCatalog` 与审批视图，前端只做薄投影，绝不逼前端拼多个接口、猜隐式约定或硬编码业务规则；
   - **保护前端帧率与内存预算**：显式区分 `Reliable`（必达终态/审批/错误）与 `Replaceable`（允许后端合帧/节流的高频流式增量），大输出一律外置引用，绝不把原始洪峰砸向 UI 主线程；
   - **消费端极简化硬标尺**：连接发现、重连、QUIC 迁移、回放尾追帧与命令幂等全部收口进 `qaqh-client`，前端接入代码净行数必须显著下降；凡是换完协议反而让前端更难写的设计，一律判定失败。
6. **RingingV3 零遗留设计铁律**（汲取 2026-09-21 → 10-30 V2 迁移 40 天拖延与 11-10 P0 活锁血泪教训）：
   - **依赖级封杀 `serde_json::Value`**：V3 契约 crate（`qaqh-ringing-v3`）禁止依赖 `serde_json`，仅用 `postcard` + 纯 Rust 强类型 `enum`/`struct` + `Bytes`，从编译期彻底断绝裸 `Value` 与字符串方法表逃生口；
   - **协议一网打尽，零协议外孤岛**：V3 必须覆盖系统观测、停机、会话、命令、Shell、Blob、Marketplace、配置、账号、配对、Gitx、FS 全部能力，严禁再遗留 `/control/v1`、`/marketplace/v1` 等孤岛端点；
   - **去版本化与零别名墙**：严禁 `ClientV3*` 重命名墙与 `#[serde(alias)]`；业务域 DTO 永久不带版本前缀，仅线缆封装带 `v3`；握手经 ALPN + 协议指纹/版本强校验，失配即拒；
   - **跨代不变量守卫先于业务代码**：必须先立 `ringing_v3_invariants`（原子订阅回放尾防活锁、四流物理隔离零队头阻塞、断联不杀进程与幂等回执、身份单权威不可伪造、多端联动原语），防止拆旧时误伤核心语义；
   - **协议迁移与存储换代物理隔离**：切 V3 全程严禁顺手改存储引擎或引入磁盘 `migrate-on-read`；
   - **异步门面解耦**：V3 传输层走纯 `async` 接口，不得直接触碰中央同步 `Mutex<AgentRegistry>`。
7. **协议迁移反腐与死刑倒计时纪律**：
   - **物理隔离建新厂**：V3 代码落独立目录（`qaqh-ringing-v3/`、`v3_server/`、`client/v3/`），CI 红线强制保证其对 `axum_server` / `RINGING_V2` / `v2_stream` / `serde_json` / `serde(alias` / `legacy_` / `compat_` **恒为 0 引用**；
   - **先冻协议再切端**：协议冻结并经 `tokio::io::duplex` 内存自闭环后才切消费端，禁止边切端边推翻核心归属模型；
   - **双栈期 V2 绝对冰封 + 硬性处决版本位**：自 `v4.6` 起新能力只上 V3、绝不倒灌 V2；双栈仅允许在 `v4.7`（切 GPUI）与 `v4.8`（切移动端）两版短暂存在，消费端单 PR 原子切换零回退开关；**`v4.9` 首张 PR 整目录物理处决 V2**，以 `rg == 0` 与不变量守卫全绿为唯一合入门禁。

---

## 第三章：代码规模管制与决策留痕

8. **上帝文件止血修正案**：`session_loop.rs`、`store.rs`、`owner.rs` 等所有非测试生产代码 **> 1,000 行** 的文件一律处于**净行数只减不增**管制，严禁向其中继续堆砌新业务分支；新能力（如 `ADR-0035` 泄洪闸等）必须独立成子模块，在既有大文件内仅允许 ≤ 10 行的薄调用接线；`SessionLoop` 彻底重写（Effect 驱动状态机 + 全异步工具 + Per-Root Actor）锁定在 **`v5.0`** 一次性完成，不做中途半吊子返工。
9. **路由单源与机器金标准**：在 V2 尚未于 `v4.9` 物理退场前，旧 HTTP/WS 路由只准在两张表（`RINGING_V2_HTTP_ROUTES` / `CONTROL_PLANE_ROUTES`）声明，出现字面量 `.route(...)` / `.nest(` / `.merge(` 即为错误；旧金标准与快照只准由机器（exporter / `--rebaseline`）生成，手改等同作弊。
10. **ADR 强制留痕与状态诚实**：所有契约、闸门、架构取舍、新引入/退役机制以及**被否决方案**一律落入 `docs/adr/` 防止反复重提。引用任何 ADR 时必须核对并显式标出状态字段，**严禁把 `已采纳（未实施）` 叙述为 `已完成`**。

---

## 第四章：分支治理、反证测试与诚实交付

11. **LTS 按线承载与契约分类治理**：LTS 属性按分支线承载而非按仓（`LTS-v3.x`、`LTS-v4.1` 坚守用户空间不可变、内核空间受控演进）；`main` 线遵循 `v4.5 → v4.12 → v5.0` 高质量演进路线，`v*` tag 不默认携带 LTS 承诺（`tag ≠ LTS`，见 `ADR-0031`），是否新设 LTS 线由所有者显式决定。触及既有 V2 冻结面的改动须由机器导出器（`check --against`）判定归属 `[contract-refreeze]` / `[guard-tightening]` / `[doc-only-refresh]`，不以提交者自报为准。
12. **高信噪比测试与反证优先**（取代旧“少写测试”条文）：禁止为了刷覆盖率堆砌无意义的样板单测，但**所有新闸门、Bug 修复、并发锁改造与协议不变量必须提供红→绿反证**（证明旧实现必红或复现病灶、新实现变绿）；CI 验收必须逐条审查**步骤明细与日志**，严禁轻信 `continue-on-error` 掩盖的顶层绿灯；验证优先官方命令与精准 `cargo test -p <涉及 crate>`。
13. **诚实交付零幻觉**：严禁因工具出错、编译/测试未完成或用户心切而默认声称已经完工；凡未实测验证的路径（如跨平台分支、真机/GUI 行为验收）必须如实写明未验范围。

---

## 第五章：宿主机资源保护、并发协同与工具纪律

14. **宿主机资源预检与防爆规程**：开工前必须采集磁盘可用空间、内存占用与 CPU 核数；在资源受限环境（如 8 GiB RAM 宿主机）须优先沟通可能出现的卡顿风险。严禁每改一行跑全仓测试或本机频繁 `--release` 全量编译；所有切片坚持单活跃平级 worktree（`../qaqh-wt-*`）、共享 `CARGO_TARGET_DIR`、精准 `-p` 验证，全量发布构建交 CI 或受控低并发（`-j1`）执行。
15. **并发协作与安全 Git 纪律**：本仓存在并发写入者，动公共文件前先 `git fetch` 核对上游与并发修改，维持 `main ⊇ LTS` 边界（撞车留痕于 ADR 而非静默 rebase 抹迹）；**全仓严禁使用 `git checkout`**，统一采用 `git worktree`、`git switch` 与 `git restore`。
16. **开工透明化与工具链纪律**：
    - 动工前必须通过任务列表（todo/steps）向用户同步修改流程；
    - 优先使用结构化 patch / edit 工具修改代码；禁止 `rg -rn` 等错误参数（`rg -r == --replace`）；使用 `sed` 修改文件必带 `rg` 核验结果；
    - 引入新库或升级关键依赖时优先查阅最新官方文档/API，避免使用过时或低效写法；
    - Shell 环境已集成 `uutils` 替代部分 `pwsh7` 实现，调用时可自然使用。
17. **Agent 实名提交修正案**：各平台 AI Agent 执行 git 提交时，优先通过 `Co-Authored-By` trailer 使用官方标准署名（如 Claude Code：`Co-Authored-By: Claude <noreply@anthropic.com>`；Codex：`Co-Authored-By: Codex <noreply@openai.com>`；Gemini / Antigravity：`Co-Authored-By: Gemini Antigravity <176961590+gemini-code-assist[bot]@users.noreply.github.com>`）；小众或无官方标准署名的 Agent 复用 Claude 或 Codex 署名即可。**唯一豁免**：QAQHarness 本身可直接以用户 git 身份提交，无需附加 AI 署名。
