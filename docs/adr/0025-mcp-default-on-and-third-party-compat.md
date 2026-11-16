# ADR-0025 MCP 默认开与三方兼容（client 侧）

状态: `已采纳` · 决策日期 2026-11-16 · 决策人：仓库所有者（口径 q1–q4）
关联：ADR-0001（kernel/user-space）、ADR-0004（strict-input）、ADR-0009（三类分类）、
ADR-0010（换锁账本）、ADR-0024（硬件控制 MCP）、ADR README #1/#3；
宪规 3/4/9/15/17/18/19/20/21。

> **状态说明**：本文记录 owner 拍板的四条口径与**已落地**的切片 A–D。
> 延后项（OAuth、SSE、更多 IDE 用户级路径）逐条列在落地清单，
> **禁止把本 ADR 读成「三方兼容已全部完工」**。

## 背景（只写已核实的事实）

### 1. client 侧链路已落地（非半成品）

- `crates/qaqh-mcp/src/adapter.rs:194-291`：stdio（进程组/Job 隔离 + 组杀兜底）、
  Streamable HTTP（自定义 headers + `${secret}` 插值）、`unix://` 三条 transport；
  `:102-107` `auto_lifecycle()` = `Auto { preferred: V_2026_07_28, legacy: Some(V_2025_11_25) }`。
- `connection.rs`：lazy connect / 10s 超时 / 5s 冷却 / crash 标记 / idle 回收 / shutdown 闸 /
  并发上限 16；连接后自动拉 `tools/list`、`resources/list`、`resources/templates/list`、
  `prompts/list`；`NotifyBridge` 订阅 `list_changed` 重拉。
- `projection.rs`（工具白名单 / category / schema 直通）、`resources.rs`（聚合 `mcp` 工具：
  list_servers / list_resources / read_resource / list_prompts / read_prompt）、
  `bridge.rs`（E-5 单一 dispatcher + 250ms 轮询 + 投影代数）。
- `service.rs:38,57`（merge → install）、`:815-843`（热重载重扫）；
  `mcp_import.rs`：A 路线只读合并 Codex / Claude-user / opencode（`import_external` 默认 true），
  B 路线 CLI 显式导入（项目级逐 server 审批、env 占位符化）。
- D4 信任边界：项目级文件绝不进只读合并（`mcp_import.rs:15-17`）。

### 2. 实证缺陷：默认开被外部导入吞掉

一次性探针（2026-11-16 本机实测，跑完即删）构造「没有手写 `[mcp]` 段、
只有用户级 Codex 配置」：

```
PROBE merged=["ext-codex-demo"] servers=["ext-codex-demo"] enabled=false
```

根因链：`config.rs:345` 的 `enabled = mcp.enabled.unwrap_or(!servers.is_empty())`
在 `merge_external` **之前**按手写 servers 折叠；`service.rs:35` 明确写
「enabled 总闸不变」；`merge_external`（`mcp_import.rs:349-373`）只 `insert` servers；
`manager.rs:215`（get_or_connect）与 `:265`（prime_all）首闸即 `mcp_disabled` ⇒
外部 server 全被拒、工具永不投影。现有 6 个 `merge_external_*` 测试全用
`base_cfg(true)`（`tests/mcp_import.rs:92-115`）手工置 true，从未覆盖该组合。
热重载路径（`service.rs:823-828`）同源。

### 3. 「不预热」与投影可见性死锁

`service.rs:77-81`、`bridge.rs:85-92`、`manager.rs:257-263` 三处写明：连接唯一
触发点是工具执行/预热，而工具必须连上 `tools/list` 才投影进模型面。去掉预热
又不给发现入口，等于「默认开了但模型一个三方工具都看不到」。这是架构冲突，
不是配置取舍。

### 4. SDK / 协议事实（本地 rmcp 3.9.0 源码 + 官方 web_fetch 2026-11-16）

- `rmcp-3.9.0/src/service/client.rs:722`：`DEFAULT_AUTO_DISCOVER_TIMEOUT = 10s`；
  本仓 `LifecycleSettings::connect_timeout = 10s`（`connection.rs:112`）。对不响应
  `server/discover` 的 legacy 三方 server，探测要等满 10s 才回退 initialize，
  外层 10s 超时会先/同时触发 ⇒ `mcp_connect_timeout` + 冷却。**未修，留痕**。
- rmcp 3.9.0 `src/transport/` 只有 `streamable_http_client` / `ws` / `child_process` /
  `async_rw`；`client-side-sse` feature 只是 Streamable HTTP **响应解码**依赖，
  不是独立 SSE client transport。
- `Cargo.toml:67` 有 `auth` feature（OAuth2 / client credentials），本仓 `qaqh-mcp`
  未启用；`handler/client.rs` 默认 `create_message` → `method_not_found`、
  `list_roots` → 空、`elicit` → decline。
- 官方 `modelcontextprotocol/servers` README：标准 transport 仅 **stdio +
  Streamable HTTP**，HTTP+SSE 已 deprecated；reference servers 自述「教学用、非生产」。
- Codex 官方 `config-reference.md`：用户级配置为 `~/.codex/config.toml`。
- Zed 官方 MCP 文档：settings 的 `context_servers` 结构（`command`/`args`/
  `environment` 或 `url`/`headers`；无 Authorization 头时走 MCP OAuth）。
- **官方搜索工具在当前 profile 未启用**；Cursor 文档页重定向回首页、VS Code
  `.md` 返回 404，Windsurf/Cline 未抓到。⇒ 这些 IDE 的**用户级文件路径未被核实**，
  本期不硬编码。

### 5. 契约面

`docs/api-surface/v4.0/types/qaqh/McpDto.ts`：`enabled: boolean`；
`McpServerDto.ts`：`transport: string`，doc 为 `"stdio" | "http"`。
改默认值的推导**不改变导出类型**（零 wire）；新增 transport 枚举值 = 取值集合
实变 = `[contract-refreeze]`，禁入冻结线，须开新 LTS 线并 bump revision。

### 6. 仓库状态（宪规 19）

`git fetch --all --prune` 后：本仓**没有 `origin/LTS-v3.0`**，
`main...mirror/LTS-v3.1 = 1427 / 1417`（历史嫁接遗留的大幅分叉，非超集关系）。
`docs/adr/README.md` 中 0024 索引行是**并发写入者未提交改动**，
`0024` 文件本身未跟踪。本 ADR 不 rebase、不 merge，只在 README 索引**追加**一行。

## 决策

1. **总闸 = 运行时纯函数，不在 load 时算死**。`McpConfig.enabled` 由 `bool` 改为
   `Option<bool>`（`Some` = 用户显式声明，`None` = 未声明）；新增
   `McpConfig::effective_enabled()` = `self.enabled.unwrap_or(!self.servers.is_empty())`。
   语义即 owner q1：**有 server 即开（含外部导入）、零配置仍关**；显式
   `[mcp].enabled=false` 仍是有效 kill switch。所有运行时读点
   （`dto.rs` / `manager.rs` / `bridge.rs` / `resources.rs`）统一走该函数。
2. **`to_persistent` 回写原值而非推导值**（`config.rs`）：未声明的配置保存后
   仍是未声明，避免把一次性的推导结果固化成用户的显式意图。B 路线显式导入
   （`mcp_import.rs:420`）写 `Some(true)` —— 用户已明确启用。
3. **启动不预热**（owner q2）：删除 `prime_all_async` 的 re-export 与
   `service.rs` 启动/热重载两处调用，daemon 启动不拉起任何三方进程。
   **可见性由 `mcp` 聚合工具的 `list_tools` action 闭环**：模型首回合只看到
   钉底的聚合工具（enabled 即在场），调一次 `list_tools` 才触发
   `get_or_connect` → `tools/list` 缓存 → bump 投影代数 → 下回合 per-server
   动态工具进模型面。走既有 lazy connect 与投影管线，**不写任何转换层**。
4. **三方差异收敛到声明式单一解析器**（owner q3）：`mcp_import.rs` 用
   `build_external` + `parse_json_entry` + `collect_json_entries` 三件套取代
   Codex/Claude/opencode 三份重复扫描。判定「有 `url` 即 remote，否则
   `command`（字符串或数组）即 stdio，两者皆无 → warn 后跳过（不再静默丢弃）」；
   自动识别 `mcpServers` / `mcp` / `context_servers` / `servers` 四键。
   新增 `scan_generic_json` 公开 API 与 `ExternalSource::Generic`，接入方式是
   **声明式表项而非 per-IDE adapter**（宪规 9）。
5. **CLI 打通 generic B 路线**：`qaqh-daemon mcp import --from generic --root <json>`，
   逐 server 审批（D4 供应链面），dry-run 默认、`--exec` 才写。
6. **不硬编码未核实的用户级路径**（宪规 8）。`default_user_paths()` 仍只含
   Codex / Claude / opencode 三个已核实路径；`scan_generic_json` 的 IDE 接线
   留到路径核实后一行接入。
7. **零 wire 改动**：本期不动 `McpTransportKind`、不动两张路由表、不动
   `docs/api-surface/*`。按 ADR-0009 分类为**无契约改动**，不需
   `[contract-refreeze]`、不需 bump revision、可进 LTS（ADR-0024 决策 10 同理）。
8. **契约/闸门决策留痕**：本 ADR 兼作宪规 17 的决策落点；新配对测试
   `external_only_servers_auto_enable_gate` 按宪规 18 反证（临时把
   `effective_enabled` 改回 `unwrap_or(false)` → 测试 FAILED，已恢复）。

## 被否选项

| 选项 | 否决理由 |
|---|---|
| 在 `service.rs` 合并后补丁式重算 `enabled`（加 `enabled_explicit` 标志） | 状态折叠仍在 load 时发生，`Config::update` 热重载路径会漂移；`Option<bool>` + 运行时纯函数才是根因修复（宪规 5：不写补丁让它看起来能工作） |
| 全局 `enabled=true` 默认值、零 server 也开 | 违反 owner q1「零配置仍关」；且 `McpManager::disabled()` 槽位初值语义被破坏 |
| 保留启动预热但「只连不投影」 | 预热就是连接；与 q2「首次调用才连」直接冲突，且 8 GB 内存下自动拉起全部第三方进程易卡顿（宪规 7） |
| 为可见性写一个「占位工具 / era 转换层」 | 宪规 4/9 点名的兼容转换层懒政；`list_tools` 直接复用既有 lazy connect + 投影代数，零转换 |
| 把「兼容三方」做成 client 侧 dual-era / SSE→HTTP 转换层 | ADR-0024 被否选项已点名；client 的 `Auto` 回退是 rmcp 原生能力、不是转换层 |
| `client` 改成 Modern-only 以「对齐 ADR-0024」 | ADR-0024 约束的是**自建 server**（`D:\mijia-ohos` 独立进程），不是 client；若 client 也 Modern-only，三方 legacy server 全废（宪规 9 拧巴设计） |
| 新增 `McpTransportKind::Sse` 并把 Claude `type:"sse"` 映射过去 | 本期 q3 未选 SSE；且新增枚举值 = `[contract-refreeze]`（宪规 15），触发条件未发生不切线。改为在解析时**明确 warn**：旧版 HTTP+SSE 未支持、按 Streamable HTTP 尝试可能连不上，不假装能用（宪规 5） |
| 为每个 IDE 写一份 `scan_*` adapter | 声明式单一解析器已覆盖四种键；adapter 堆叠是宪规 9 的懒政 |
| 凭印象硬编码 Cursor / VS Code / Windsurf / Cline 的用户级路径 | 官方文档未核实到；宪规 8 禁止因工具错误声称完工 |
| 本期就切新 LTS 线并 bump revision | 零 wire 改动，切了是无用的破坏性动作（宪规 2 不拖泥带水）。owner q4 的授权存档于下方，等 SSE 真落地时执行 |

## owner 口径存档（防重提）

| 编号 | 口径 | 本期处置 |
|---|---|---|
| q1 | 有 server 即开（含外部导入），零配置仍关 | 已落地（决策 1） |
| q2 | 允许合并但不预热，首次调用才连 | 已落地 + 架构冲突解法（决策 3） |
| q3 | 优先补外部配置解析面（Claude/Codex 新形态、更多 IDE） | 格式层已落地；IDE 用户级路径待核实（决策 4/5/6） |
| q4 | **允许**新增 transport 枚举值 → 开新 LTS 线并 bump revision | **授权存档、本期未触发**（零 wire）。SSE 落地时按宪规 15/17：先 `[contract-refreeze]` 分类、读 ADR-0010 账本、开新线 bump revision、按宪规 18 反证闸门会红 |

## 追加决策 R1：rmcp 统一到 3.10.0（2026-11-16）

owner 指令「rmcp 统一用最新版本」+「有什么冲突？」的落点。

### R1.1 决策

workspace `[workspace.dependencies].rmcp` 精确锁 **`=3.10.0`**（不用 `^`），
`Cargo.lock` 同步到 3.10.0，所有注释里的历史版本戳统一为 3.10.0。
3.10.0 = crates.io `max_version` / `newest_version` / `max_stable_version`
（2026-11-12 发布，`web_fetch` 核实）。

### R1.2 核实事实（升级前）

| 项 | 3.9.0 | 3.10.0 | 结论 |
|---|---|---|---|
| src 差异 | — | 仅 5 文件改 + 新增 `handler/server/skill_catalog.rs` | 极小 |
| `streamable_http_client.rs` pub API | 2098/2105/2149 | **行号完全相同** | 零破坏 |
| `from_unix_socket` | `common/unix_socket.rs:538` | 同 | 零破坏 |
| `ClientLifecycleMode` / `serve_with_lifecycle` | `service/client.rs:707/726` | 同 | 零破坏 |
| `ClientHandler`（`handler/client.rs`） | — | **文件未改** | 零破坏 |
| `service.rs` | — | 只加 doc 注释（+20 行） | 零破坏 |
| `transport/io.rs` | — | 只加 4 行 doc | 零破坏 |
| `mcp_headers.rs` | — | schema 校验放宽 + 测试 | 不涉公开 API |
| `handler/server.rs` | — | 只加 `#[cfg(feature="skills-fs")] pub mod` | 未启用该 feature |
| `DEFAULT_AUTO_DISCOVER_TIMEOUT` | 10s | **仍 10s** | 「10s 撞车」待办仍成立 |
| 依赖版本 | reqwest 0.13 / process-wrap 10.0 / schemars 1.2.3 | 同 | 本仓无需改 |
| rust-version / edition | 1.88 / 2024 | 同 | 本仓 edition 2024 ✓ |

`cargo update -p rmcp --precise 3.10.0 --dry-run` 与实际执行均**只更新
`rmcp` 一行**（version + checksum），不新增 `cap-std` / `yaml_serde`
（`skills-fs` 的 optional 依赖未启用），`rmcp-macros` 不在 lock 中
（`default-features = false` 且未开 `macros` feature）。

### R1.3 冲突清单（owner 问「有什么冲突」）

1. **四处版本戳不一致**（要统一的对象，不是技术冲突）：
   `Cargo.toml:53` `^3.2`、`Cargo.lock` 3.9.0、
   `qaqh-mcp/Cargo.toml` + `qaqh-lsp/Cargo.toml` 注释 3.5.1、
   `qaqh-mcp/src/adapter.rs` 注释 3.2.0。
2. **`^` 漂移语义**：`version = "3.2"` 是 caret，本机 registry 已出现
   `rmcp-3.10.0` 源码就是它曾解析到 3.10.0 的证据；ADR-0024 落地清单也记录过
   「`version = "3.9.0"` 被 `^` 解析为 3.10.0，改精确锁」的同类事故。
   ⇒ 只改 lock 不改声明，下次 `cargo update` 又会漂；必须 `=3.10.0`。
3. **ADR-0024 的 3.9.0 口径（文档冲突，非编译冲突）**：0024（未跟踪、
   并发写入者持有，状态 `已采纳（未实施）`）写「新 crate 必须锁 3.9.0，否则
   出现两个 `ServiceExt`/`ProtocolVersion` 类型」。但 0024 的 crate 是
   `D:\mijia-ohos\home-control-mcp` **外部独立进程、完全不进本仓 workspace**
   （0024 决策 1），两者不共享 Rust 类型空间 ⇒ 本仓升 3.10.0 **不会**产生
   双实例。按宪规 19 不悄悄改并发写入者的文件：**本次不改 0024 内容**，
   在此留痕——若日后把 `home-control-mcp` 移入本仓 workspace，必须同步到
   3.10.0，否则才会出现 0024 担心的双实例。
4. **ADR-0001「5 处版本戳」的误判**：0001 §后果指的是 **workspace 版本号**
   （`version.txt` / `Cargo.toml` / `Cargo.lock` / `package.json` /
   `qaqh-backend.lock.json`），不是 rmcp。升级 rmcp 不动这五处 ⇒ 不撞
   `--locked` exit 101。这是最容易误报为冲突的一条。
5. **`Cargo.lock` 字节变化与契约面**：lock 不在冻结面前缀（0001 §1：
   docs/api-surface / ringing-contract / ringing/src/v2 / command.rs /
   qaqh-client / contract-export）⇒ 按 ADR-0009 **零 wire 实变**，
   不需 `[contract-refreeze]`、不需 bump revision、可进 LTS。
6. **LTS 不变量**：`git fetch --all --prune` 后本仓无 `origin/LTS-v3.0`，
   `main...mirror/LTS-v3.1 = 1427 / 1417`（历史嫁接遗留分叉）。按宪规 19
   不 rebase、不 merge，只留痕；本次改动是内核空间依赖升级（宪规 21 允许）。
7. **官方契约检查**：升级前 `check --against docs/api-surface/v4.0` 已 exit 1
   （18 项既有漂移，commit `41b16c62`）。升级后重跑，若差异列表**逐项相同**
   即证明未引入新漂移（见落地清单）。
8. **资源**：8 GB / 8 核，3.10.0 源码已在本机 registry，增量编译；
   按宪规 7/10 不跑 `--release` 全量构建。

### R1.4 被否选项（追加）

| 选项 | 否决理由 |
|---|---|
| 保留 `^3.2`，只 `cargo update` 到 3.10.0 | 声明与锁仍不一致，下次 update 又漂；宪规 4 要求砍掉版本戳不一致 |
| 用 `^3.10` / `"3.10"` | 同样是 caret；ADR-0024 已记录 `^` 漂移事故，精确锁才是 LTS 仓口径 |
| 升到 3.9.0 中间版或自定版本 | owner 口径是「最新版本」，crates.io 只有 3.10.0 是最新稳定 |
| 直接改写并发写入者持有的 ADR-0024 内容 | 宪规 19 撞车留痕，不悄悄 rebase；且 0024 的外部 crate 不在本仓，改它不是「统一本仓」的必要动作 |
| 因「0024 说锁 3.9.0」而放弃升级 | 0024 约束的是外部独立进程，不共享类型；把它读成全局禁令是误读（宪规 9） |

## 追加决策 R2：不砍 `qaqh-skills` 去复用 MCP Skills 扩展（2026-11-16，被否留痕）

owner 提问「是不是可以直接不写自己的 `qaqh-skills`，复用它家的标准？」的落点。
按 ADR README #1/#3 与宪规 3/9/17/21，此提案**被否**，在此留痕防重提。

### R2.1 结论

**不砍。**「砍自研、全盘改走 MCP Skills」是架构倒退且触碰已冻结的用户空间契约。
真正值得「复用它家标准」的是**格式层**，而格式层**已经统一**：
`SKILL.md` + `---` YAML frontmatter（`name`/`description`）与 MCP
`SkillFrontmatter { name, description, extra }` 逐字段一致，一个技能文件都不用改。

### R2.2 事实：`qaqh-skills` 不是可替换的孤立模块（已核实）

| 层 | 现状 | 落点 |
|---|---|---|
| 模型面 / system prompt | `render_system_section()` 把 **bundled 技能正文编译期注入**，磁盘技能只注入元数据目录（渐进披露） | `qaqh-agent-runtime/src/session_agent/prompt.rs:37`；`qaqh-skills/src/lib.rs:428-470` |
| 模型面工具 | 唯一工具 `skill_list`；注册 + 能力表 + schema 断言 + 执行测试 | `qaqh-workspace/src/skill.rs`、`tool_capabilities.rs:108`、`schema_spot_check.rs` |
| 权限安全边界 | `skill_roots()` 判定「instruction injection 路径」，**不享受 Level-3 自动批准** | `qaqh-permission/src/permission.rs:415-424` |
| 控制面 / 冻结导出物 | `skills.catalog`、`skills.list_tools` 的参数/结果 DTO 在 `docs/api-surface/v4.0/manifest.json`（ts/cs/fixtures 齐全） | `qaqh-runtime/src/ringing/service_methods.rs:197-223` |
| FFI | `skills_catalog_json()` 导出给外部宿主 | `qaqh-ffi/src/lib.rs:326` |
| 历史决策 | ADR-0010 记录 2026-11-12 裁剪激活状态机三型，但**刻意保留** skills 服务端点与参数类型（v3.1 机器重切吸收） | ADR-0010 §191-199 |
| 跨宿主正典 | skills 是 qaqh/codex/claude 三宿主共用的**中立格式**，不是 MCP 专属 | ADR-0022 决策 1/4 |
| 已有同类评估 | `DESIGN.md` 分析过 Codex `skills.list`/`skills.read`（含 package authorities、pagination），**明确决定保留 QAQH 的 `skill_list`** | `qaqh-skills/DESIGN.md:84` |

bundled 技能共 14 个文件，`include_dir!` 编译期嵌入二进制、**不落盘**
（`qaqh-skills/src/lib.rs:15-17`）。

### R2.3 「复用 MCP 标准」的运行时前提

- Skills over MCP 是 **opt-in Extension**，官方 2026-07-28 spec 原文
  「always opt-in and require explicit support from both client and server,
  negotiated through capabilities」；核心 server features 只有
  Resources/Prompts/Tools(+Discovery)。
- rmcp 的 `SkillCatalog` 是 **server 侧**实现（`skills-fs` feature），
  `from_dir(磁盘目录)` + `cap_std` 沙箱。要用它必须**先有一个 MCP server 进程**
  托管 skills 目录，再由 client 通过 `skills/list`/`skills/get` +
  `resources/read` 走 JSON-RPC 往返消费。
- 本仓 `qaqh-mcp` 是**纯 client**，本仓没有 MCP server；ADR-0024 决策 1 已定
  自建 server 是 `D:\mijia-ohos` 独立进程、完全不进本仓 workspace。

### R2.4 被否理由

1. **位置不对（架构倒退）**：把「零进程、零网络、编译期嵌入、直接注入
   system prompt」的宿主内核能力，换成「依赖外部 server + 能力协商 +
   JSON-RPC 往返」的外部能力。daemon 启动就要多拉一个进程，8 GB 机器上
   凭空增加故障点与启动复杂度（宪规 7）。
2. **bundled 覆盖不了**：`SkillCatalog::from_dir()` 只读磁盘目录，读不到
   `include_dir!` 嵌在二进制里的 14 个文件。要么把 bundled 落盘（破坏既有
   设计），要么自己实现 server handler —— 那等于**还是在写 skills 运行时**，
   只是换了层皮。
3. **把中立格式绑死到 opt-in 扩展**：ADR-0022 的三宿主共用正典会收窄成
   「只有声明了该 extension 的 server 才认」，是收窄不是统一。
4. **触碰冻结面（宪规 15/21）**：`skill_list` 工具、`skills.catalog`/
   `skills.list_tools` 的 DTO 与 fixtures、FFI 导出、system prompt 注入、
   权限 roots 判定都是**用户空间/模型面**。砍掉 = wire 实变 ⇒
   `[contract-refreeze]` ⇒ 禁入冻结线、须开新 LTS 线 + bump revision，
   且直接改前端与模型行为。LTS 仓不允许。
5. **宪规 9**：任何「保留旧面但加一层 MCP 桥」或「砍掉但写适配层把 MCP
   skills 喂给旧 `skill_list`」都是被点名的兼容转换层懒政，明确不做。
6. **仓里已有同口径否决**：`DESIGN.md:84` 对 Codex 的 `skills.list`/
   `skills.read` 已评估并决定保留自有面；MCP SEP-2640 只是换了传输，不改变
   「skills 运行时属于宿主内核」这一判断。
7. **`skills-fs` 不需要开**：它是 server 侧 feature，本仓 client 开了只会把
   `cap-std`/`yaml_serde` 拖进依赖图（R1.2 已核实）。

### R2.5 更好的架构（宪规 3/4 应推行的）

**一个格式、两个原生方向、零转换层**：

- 格式层：继续用 `SKILL.md` + frontmatter（已与 MCP 完全一致），三宿主共用。
- 对外发布（可选）：用 rmcp `SkillCatalog` 做**独立进程的 exp 接线**
  （宪规 21 允许内核空间以 exp 设计接线），不替换内核、不加转换层。
- 收三方 skills（可选）：走 client 侧**原生发现**（`ConnState.skills` +
  `list_all_skills()` + 聚合工具 action + `skills/list_changed`），不转换。

这两个方向都是**增量**，不动任何现有契约；`qaqh-skills` 保留为宿主内核的
极简运行时（零进程、文件系统唯一事实源）。

### R2.6 若将来重提

先回答三个问题：bundled 正文如何不落盘地提供？权限层的 instruction-injection
roots 由谁维护？`skills.catalog` / `skill_list` 的用户空间契约按哪条 LTS 线
refreeze？三个都答不上来，就不要动运行时。

## 后果

- 用户空间行为变化：`ConfigDto.mcp.enabled` 的**运行时值**在有外部 server 时由
  false 变 true；DTO 类型/字段/导出物字节不变（已用官方 check 验证，见落地清单）。
- `McpConfig.enabled` 是 `Option<bool>` —— 所有全字段字面量构造点（14 处测试 +
  `import_servers` + `apply_config` 测试）已同步；后续新增构造点必须用 `Some/None`。
- `prime_all_async` 已从公开 re-export 移除；`McpManager::prime_all()` 保留为
  可复用的批量连接 API（无启动调用者）。
- 反证（宪规 18）：临时把 `effective_enabled()` 改回旧行为 →
  `cargo test -p qaqh-config --test mcp_import external_only_servers_auto_enable_gate`
  **FAILED**（断言「有 server（含外部导入）即开」）；恢复后 PASS。看的是测试明细，
  不是 job 绿灯。
- 脆弱点（**未修、留痕**）：rmcp Auto 探测 10s 与 `connect_timeout` 10s 撞车；
  legacy server 若静默忽略 `server/discover`，每次连接都会超时 + 冷却。
  修法应是把探测超时显式压到 connect_timeout 之下，并用
  `tests/fixtures/mcp_stdio_server.mjs` 做真实 legacy 冒烟。
- 仍缺（**未做**）：OAuth 2.1 / PKCE（rmcp `auth` feature 未启用）、
  旧版 HTTP+SSE transport、client 能力面（sampling / roots / elicitation）、
  更多 IDE 用户级路径接线。
- 版本戳**已统一**（追加决策 R1）：workspace `rmcp = "=3.10.0"`、注释 3.10.0、
  `Cargo.lock` 3.10.0；`reqwest 0.13` / `process-wrap 10` 与 3.10.0 对齐，
  全图单一 `ServiceExt` 类型（ADR-0024 §后果点名的同类事故不再可能）。
- 资源：本机 8 GB / 8 核（约 2 GB 空闲）。按宪规 7/10 不跑 `--release` 全量
  构建，验证只走窄命令；默认开不再启动预热，正好规避多进程拉起的卡顿。
- 并发写入者：`docs/adr/README.md` 的 0024 索引行未提交；本 ADR 只在其后
  **追加** 0025 行，不覆盖他人改动（宪规 19 撞车留痕）。
- **官方契约检查如实结果**（宪规 8/20）：`cargo run -p qaqh-contract-export --
  check --against docs/api-surface/v4.0` 返回 **exit 1**，`18 artifacts differ
  byte-wise`（constants.ts / coverage.json / RingingCommand / SessionEvent /
  SessionState 及若干 fixtures）。核实：`docs/api-surface/` 无未提交改动，
  本次改动文件不含 `qaqh-types` / `qaqh-ringing-contract` / exporter，
  18 项的最近提交为 `41b16c62`（改契约面未重切金标准，ADR-0011 场景），
  **属 main 上既有漂移、与本次改动无关**；差异列表不含 `McpDto.ts` /
  `McpServerDto.ts` / `ConfigDto.ts` ⇒ 本次改动零 wire 变化。
  这 18 项不因本 ADR 变绿，也不由本 ADR 修复（宪规 8）。

## 落地清单

- [x] `McpConfig.enabled: Option<bool>` + `effective_enabled()` + `to_persistent` 回写原值
- [x] 全部运行时读点（dto/manager/bridge/resources）+ 14 处测试字面量同步
- [x] 配对闸门 `external_only_servers_auto_enable_gate` + 反证（临时插违规 → FAILED）
- [x] 去启动预热（service.rs 两处 + lib.rs re-export），`mcp list_tools` 按需发现
- [x] 声明式解析面（build_external / parse_json_entry / collect_json_entries /
      scan_generic_json / ExternalSource::Generic），Claude streamable-http、Codex url、
      Zed context_servers 三形态测试
- [x] CLI `--from generic --root <json>` 走 B 路线逐 server 审批
- [x] `cargo check -p qaqh-mcp --all-targets` / `-p qaqh-runtime` / `-p qaqh-daemon`
- [x] `cargo test -p qaqh-config` / `-p qaqh-mcp` / `--test mcp_import`（15 项）
- [x] rmcp 统一到 3.10.0：精确锁 `=3.10.0` + `Cargo.lock` + 四处版本戳 +
       ADR 留痕（见追加决策 R1）
- [x] R2 被否方案留痕（不砍 `qaqh-skills`、不写转换层）；**无代码 / 无契约改动**
- [x] 官方 `check --against docs/api-surface/v4.0`：差异列表 18 项均为既有漂移、
      不含 MCP/Config DTO ⇒ 本次改动零 wire 变化（见「后果」如实记录，exit 1）
      ；**升级 3.10.0 后重跑，18 项逐项相同**（未引入新漂移）
- [ ] main 上既有 18 项契约漂移（commit `41b16c62` 改契约面未重切金标准，
      ADR-0011 场景）单独立项处理，不在本 ADR 范围
- [ ] 解 Auto 探测 10s 与 connect_timeout 10s 撞车 + legacy 真实冒烟
- [ ] OAuth 2.1 / PKCE：启用 rmcp `auth` 原生流程（不自己写 OAuth 层）
- [ ] SSE：按 q4 授权开新 LTS 线 + `[contract-refreeze]` + bump revision + 反证
- [ ] client 能力面：声明 capabilities + sampling / roots / elicitation
- [ ] 核实 Cursor / VS Code / Windsurf / Cline 用户级路径后，一行接入 `scan_generic_json`
- [ ] `import_external` 暴露到 `McpDto`（前端可见/可关外部导入）
