# 工具模式（标准/极限/创造）+ Shell 独立工具（计划）

> 状态：实施中（4.1-4.4 + 前端接口已落地，2026-08-15；4.5 输入面板三选一已落地，创造模式精细勾选/模式徽标待做）
> 范围：`crates/deepx-workspace`（`set_allowed_tools`、bash/pwsh 独立工具注册）、`crates/deepx-session`（`tool_mode` 元数据）、`crates/deepx-msglp`（session 应用 + `tool_defs` 刷新）、UI（输入面板/精细 session 配置）
> 关联：[PLAN-EDIT-FILE-V2.md](./PLAN-EDIT-FILE-V2.md)、[PLAN-SESSION-EDIT-LEASE.md](./PLAN-SESSION-EDIT-LEASE.md)（session 元数据持久化同款模式）、`crates/deepx-workspace/src/exec.rs`（现有 Shell 抽象）

## 1. 结论 / 决策

- **三模式 = 三张 allowed 工具表**，复用现成 allowlist 链路（`init_tools` → `manager.allowed` → `filtered_defs` 源头过滤 → `prepare_req` 执行层拦截），只补一个**运行时** `set_allowed_tools` 入口。主代理现状是全量（"all tools, no allowlist"），subagent 已用同一机制——本计划把 allowed 从"subagent 专用"提升为"会话级可配置"。
- **模式定义**：
  - 标准（Standard）= 全工具（`allowed = None`，现状行为）；
- 极限（Minimal）= **档位化**（见 D2）：A（8 工具）/ B（6 工具）/ C（4 工具），均含 `confirm_apply` 保证 edit/write 的 dry_run→确认→落盘闭环；
  - 创造（Custom）= 用户勾选（预设基底 + 增删，见 D6）。
- **bash/pwsh 独立工具**：复用 exec 引擎（`exec.rs` 的 `Shell` 抽象已齐），各自注册为固定 shell 的工具；`exec` 保留作通用入口（argv 无 shell 直跑 + command 默认 shell 包装 + `shell` 参数兼容）。
- **软检测，拒绝"缺工具即拒启动"**：注册照常进行；调用时解析失败才报错，错误信息列出本机可用 shell（反模式对照：有的 agent 检测不到 git-for-windows 直接不允许启动）。
- **降级链**：`pwsh` 工具内部 pwsh7 → `powershell.exe` 兜底（反模式对照：有的 agent 非得 pwsh，cmd/powershell5 都不行）。
- **工具描述动态注入本机 shell 探测结果**：注册时探测一次写入 description（如 `bash → C:\Program Files\Git\bin\bash.exe`），模型从 tool list 即知环境。
- **粒度 = per-session 持久化**：`tool_mode` 存 session 元数据，session 恢复时应用；输入面板只是改元数据的入口。进程级 `TOOL_MANAGER` 是 OnceLock 单例，多 session 并存时靠"切换 session 重设 allowed"实现隔离。
- **与 agent mode（plan/code）正交**：`tool_mode` 管允许集，`AGENT_MODE` 管 `PLAN_BLOCKED` 纵深拦截；叠加生效 = 允许集 − PLAN_BLOCKED（plan 模式）。
- **`PLAN_BLOCKED` 保留**作纵深防御，不删除；三模式不替代 plan/code 语义。

## 2. 现状（代码实证，2026-08-15）

- ✅ allowlist 链路已完整：`apply_init`（`crates/deepx-workspace/src/manager.rs` L92，known 过滤 + 未知名 warn）、`filtered_defs`（L126，allowed 非空时过滤 ToolDef）、`prepare_req`（L150，执行层拦截 "not in the allowed list for this subagent"）、`init_tools`（`src/runtime.rs` L113）。
- ✅ 模型侧注入点 = `agent.tool_defs`：主代理 `init()` L305 `all_tools()`（注释 "all tools, no allowlist"）；subagent `init_subagent()` L325 传 allowlist + L328 刷新；session 恢复时 `lifecycle.rs` L129 刷新——**唯独 `set_mode` 不刷新 tool_defs**（`ringing_v1/engine_misc.rs` L93-104），这是"源头白名单"要补的缺口。
- ✅ Shell 抽象已从 codex-rs 移植（`src/exec.rs`）：`enum Shell` L28、`detect` L44（Windows：pwsh7 → powershell.exe → git-bash/MSYS2 → cmd）、`from_name` L52（Windows `bash` 解析 git-for-windows/MSYS2 并**避开 WSL wrapper**，候选 L57-67）、`find_bash_on_path` L206（跳过 System32/WindowsApps）、`derive_exec_args` L148（`bash -c` / `pwsh -NoProfile -Command` / `cmd /c`）、路径缓存 `DETECTED_BASH_PATH`/`DETECTED_PWSH_PATH` L38-40。
- ✅ exec 注册：`src/exec.rs` L1037-1068（key `exec`，schema 含 `shell` 参数 L1047，handler `handle_run`，category Exec）。引擎与注册分离点明确，拆独立工具 = 换 key + 固定 Shell + 去 `shell` 参数。
- ✅ session 元数据持久化先例：`persist_mode`（`crates/deepx-session/src/manager.rs` L231）、恢复点 `engine_session.rs` L56 `set_mode(saved_mode)`。
- ✅ `edit_file_v2` **已支持创建新文件**：文件不存在 + 无 `expected_hash` → 空内容进正常管线（`file_edit_v2.rs` L1425-1428）；`PrependFile` 锚定位置 0 / `AppendFile` 锚定 `content.len()`，空文件上 Tier1 必中（L773-790）；写盘统一 `atomic_write`（L1560）。**缺**：整文件覆盖 kind、创建路径正式测试、file_state ledger 记录、NO_MATCH 创建提示。
- ⚠️ `manager.allowed` 只在 init 时设置（`apply_init`），无运行时 setter——需新增。
- ⚠️ `set_mode`（`engine_misc.rs` L93）只改 `AGENT_MODE` + persist，不刷新 `tool_defs`。

## 3. 决策点

| # | 事项 | 建议 | 截止 |
|---|---|---|---|
| D1 | 模式粒度 | **per-session 持久化**（session 元数据 `tool_mode`），session 切换/恢复时应用；输入面板只是入口 | 定稿 |
| D2 | 极限模式工具集 | **档位化**：A = `[bash, pwsh, write, edit, read, glob, grep, confirm_apply]`（8 个，默认；`read` 为建议补充：`edit` 的 `expected_hash` 来自 `read`，教学链路 read→edit→verify，缺它则 hash 校验 UX 全退化）；B = `[bash, edit, glob, grep, read, confirm_apply]`（6 个，**简化档**——测试模型在极小工具集下的能力，模型 token 花在推理而非 shell 拼写）；C = `[bash, edit, glob, confirm_apply]`（4 个，压力测试档）。**每档含 `confirm_apply`**：edit/write 支持 dry_run→pending_id→confirm 确认闭环，缺它则模型被 dry_run 协议引导却无法落盘（其他模型尤甚）。`tool_mode` 编码：`minimal` = A、`minimal:b` = B、`minimal:c` = C | 定稿 |
| D3 | shell 工具形态 | `bash`/`pwsh` 独立工具 + `exec` 保留通用入口；**cmd 不独立**（Windows 专属、语义弱，需要时 `exec shell="cmd"`） | 定稿 |
| D4 | 检测策略 | **软检测**：注册不拒绝；调用时解析失败报错 + 列出可用 shell；`pwsh` 工具 pwsh7→powershell.exe 降级链 | 定稿 |
| D5 | 描述注入 | 注册时探测一次写入 description（复用 `DETECTED_*_PATH` 缓存）；探测结果稳定则 tool schema 稳定，prompt cache 影响最小 | 实施时 |
| D6 | 创造模式 UX | **预设为基底 + 勾选增删**（从零勾选易漏基础工具导致模型半瘫）；`skills` 默认勾选 + 取消时警告（skill 激活机制整体失效） | 实施时 |
| D7 | 纵深防御 | `PLAN_BLOCKED` 保留；`tool_mode` 与 `AGENT_MODE` 正交叠加（允许集 − PLAN_BLOCKED） | 定稿 |
| D8 | prompt cache | 切模式刷新 `tool_defs` 破坏前缀缓存——接受（与 mode 切换同代价，plan/code 切换本就要换上下文） | 定稿 |
| D9 | UI 入口形态 | 输入面板选择器（三选一）+ 精细 session 创建时预设；具体控件方案待 UI 团队确认 | UI 动工时 |
| D10 | edit 支持"写" | **`edit_file_v2` 补 `kind: "overwrite"`**（`new` 给全文、无 `old`，整文件覆盖；等价 write 语义）——档位 C 前置。配套：a) 创建路径（prepend/append + 不存在文件）补正式测试；b) NO_MATCH 提示"文件不存在，改用 prepend_file/append_file 创建"；c) file_state ledger 补记录（write 有 `record_write`，v2 目前不记）。完成后 `write` 可退役（保留兼容） | 定稿 |

## 4. 实施细节

### 4.1 workspace：`set_allowed_tools` 运行时入口

- `runtime::set_allowed_tools(tools: Vec<String>)`：`with_manager(|m| m.set_allowed(tools))`；
- `ToolManager::set_allowed`：复用 `apply_init`（L92-120）的 known 过滤逻辑（未知名 warn 丢弃）；空列表 = `None`（全量，标准模式）；
- `filtered_defs`（L126）无需改动——already allowed 感知；
- `prepare_req`（L150）拦截无需改动——already 纵深。

### 4.2 bash/pwsh 独立工具（exec.rs 内）

- 抽出注册函数 `register_shell_tool(mgr, key: &str, shell: Shell)`，与 `register`（L1037）共享同一 handler 与 schema 模板，差异：
  - schema **去掉 `shell` 参数**（工具名即 shell 语义），其余参数（argv/command/cwd/env/timeout_secs/background_after_secs/max_output_tokens）不变；
  - description 前缀注明固定 shell + 本机解析路径（探测结果动态注入）；
  - `bash` 工具 = `Shell::Bash`（Windows 走 git-bash 解析链 L57-67/L206，避开 WSL wrapper）；`pwsh` 工具 = `Shell::PowerShell`（降级链 pwsh7→powershell.exe）；
  - 调用时解析失败（无 bash / 无 pwsh 且无 powershell.exe）→ `ToolResult::error("bash not found on this machine; available shells: pwsh, cmd")` 风格；
- `exec` 保留（argv 无 shell + command 默认 shell + `shell` 参数兼容旧用法）；三工具 category 均为 `ToolCategory::Exec`（permission.rs L18 分类不变）。
- 工具名进入注册表后自动被 allowlist 机制覆盖（`bash`/`pwsh` 可直接出现在极限模式白名单里）。

### 4.3 session 元数据（deepx-session）

- session meta 新增字段：`tool_mode: String`（`"standard" | "minimal" | "minimal:b" | "minimal:c" | "custom"`）+ `custom_tools: Vec<String>`（仅 custom 用）；serde default 兼容旧 session（缺失 = standard）；
- 持久化走 `SessionManager`（`persist_mode` L231 同款：`persist_tool_mode(seed, mode, custom_tools)`）；
- 恢复点：`engine_session.rs` L56 附近（`set_mode(saved_mode)` 同处）应用 `tool_mode`。

### 4.4 msglp：应用与刷新

- `AgentState` 增加 `tool_mode` 字段（与 `tool_defs` 同层）；
- 应用函数 `apply_tool_mode(&mut self)`：`set_allowed_tools`（standard → 空；minimal → 固定表；custom → 勾选表）→ `tool_defs = all_tools()` 刷新；
- 调用点：`init()`（主代理默认 standard，L305 之后）、`init_subagent()`（subagent 保持显式 allowlist，不叠加 tool_mode）、session 恢复（`lifecycle.rs` L129 处）、输入面板切换（经 daemon/命令通道到 msglp）；
- subagent 与 tool_mode 的关系：subagent 白名单（L325 传入）在 manager.allowed 层生效，tool_mode 也写同一字段——**subagent 场景以显式 allowlist 为准**，不读 session tool_mode（避免叠加混乱；如需叠加再评估）。

### 4.5 UI

- 输入面板：模式选择器（标准 / 极限·8 / 极限·6 / 极限·4 / 创造），切换即时生效 + 持久化（D9 确认控件形态）；**✅ 已落地（2026-08-15）**：composer footer `composer-tool-mode` ComboBox 五选一 → `session.set_tool_mode`（乐观更新 + meta.json 回填）；标签数字与后端档位工具数保持一致；
- 创造模式：勾选器（预设基底 = 标准或极限 + 增删；`skills` 取消时警告）——**🔲 待做**（当前 MVP：预设基底 = 全量注册工具，切换即生效）；
- session 列表/会话头显示当前模式徽标（复用 `session_rev` 刷新机制，见 PLAN-SESSION-EDIT-LEASE.md 4.3 同款）——**🔲 待做**。

### 4.6 边界与安全

- 极限模式**不是更安全**：`bash`/`pwsh` 是 `ToolCategory::Exec`（High risk，permission.rs L72），给了 shell 即任意命令能力；安全边界仍靠 workspace 边界 + 权限级别，工具数量只是减少模型分心；
- 切模式时正在执行的调用不受影响（`prepare_req` 在调用起点检查）；
- 未知工具名进白名单 → known 过滤 + warn（复用 apply_init 语义），不静默吞掉。

### 4.7 `edit_file_v2` overwrite（D10，档位 C 前置）

- 新 hunk kind：`{"kind": "overwrite", "new": "<全文>"}`——不要求 `old`，定位恒成功（等价 write 的 content 覆盖语义）；与 replace 的差异 = 无匹配负担，适合整文件重建；
- 文件不存在时 overwrite = 创建（与 prepend/append 创建路径同基线：L1425-1428）；
- hash gate：overwrite 同样吃 `expected_hash`（防覆盖竞争写）；新文件创建沿用"无 hash 直接过"；
- file_state：创建/覆盖后补 `record_write`（对齐 write 工具台账）；code_delta 的 `edit_file_v2` 分支（`src/code_delta.rs`）补 overwrite 计数（`new` 行数 → added）；
- hint 扩展：`NO_MATCH` 且文件不存在 → "file does not exist; create with prepend_file/append_file (or overwrite)"；
- 完成后评估 `write` 退役（注册表保留、教学改为 overwrite；白名单档位 B/C 不再需要 write）。

## 5. 测试点

- workspace：`set_allowed_tools` 空=全量 / 已知过滤 / 未知名 warn 丢弃；切 allowed 后 `filtered_defs` 随之变化；`prepare_req` 拦截非白名单（复用现有 subagent 拦截测试改参）；
- workspace：`bash`/`pwsh` 注册后 schema 无 `shell` 参数、description 含探测路径；Windows 上 `bash` 工具解析到 git-bash 路径（避开 System32/WSL）；不可用 shell 调用报错含可用替代列表；
- msglp：session 恢复应用 `tool_mode`（standard/minimal/custom 三态）+ `tool_defs` 刷新；custom 持久化 `custom_tools` 往返；
- 集成：minimal 模式下 `agent.tool_defs` 只含 7 工具；minimal:b 只含 5 工具、minimal:c 只含 3 工具；模型侧看不到被禁工具（源头）；plan 模式叠加 PLAN_BLOCKED 仍拦截 write 类；
- 兼容：旧 session 无 `tool_mode` 字段 → 默认 standard，不 panic。
- edit_file_v2：overwrite 创建/覆盖/带 hash 防竞争；prepend/append 创建新文件（Tier1 必中）；误用 replace 于不存在文件 → NO_MATCH + 创建提示；file_state 台账记录对齐 write。

## 6. 变更日志

- 2026-08-15：定稿初版（三模式 + bash/pwsh 独立工具 + 软检测/降级链策略，来源：设计讨论 + exec.rs Shell 抽象实证）。
- 2026-08-15：修订一——极限模式档位化（A/B/C）；新增 D10（edit overwrite，档位 C 前置）；现状补 edit_file_v2 创建路径实证（L1425-1428/L773-790/L1560）；实施细节补 4.7。
- 2026-08-15：**实施**——D10 overwrite（file_edit_v2.rs：Overwrite hunk/独占检查/创建提示/record_write，workspace 239 tests）；4.1 set_allowed_tools（manager.rs set_allowed + runtime 入口）；4.2 bash/pwsh 独立工具（exec.rs handle_run_with_shell + register_shell_tool + 软检测 + 描述注入）；4.3 SessionMeta.tool_mode/custom_tools + persist_tool_mode；4.4 ControlCommand::SetToolMode + AgentState::apply_tool_mode + lifecycle 恢复接入；前端接口 `session.set_tool_mode`（daemon service.rs，persist + Control 频道应用）；测试：types 2 + msglp 1。UI（4.5）与档位 B/C 切换 UI 待做。
- 2026-08-15：**实施——前端接线**：`deepx-client` `ActionRequest::SessionSetToolMode`（`session.set_tool_mode`，含测试）；`bridge.rs` `ComposerState.tool_mode/tool_mode_custom_tools` + `composer_tool_mode` 缓存 + `sync_tool_mode_from_meta`（meta.json 回填，250ms 轮询可见）+ `spawn_set_tool_mode`；`composer_bar.rs` footer 三选一 ComboBox（`composer-tool-mode`，防程序化同步误触发同 permission）。创造模式精细勾选/模式徽标待做。
- 2026-08-15：**实施——档位 B/C + 工具名缩写**：`read_file`→`read`、`edit_file_v2`→`edit`（注册 key + 全仓库引用 + config 存量迁移 `file/read_file→read`、`edit_file_v2→edit`）；极限模式新增五元组 `minimal:b`（bash/edit/glob/grep/read）与三元组 `minimal:c`（bash/edit/glob），daemon 校验 + `apply_tool_mode` + composer 五选一落地；修复 daemon 进程未 `init_tools` 导致 `skills.list_tools` 空列表、`apply_tool_mode` 未同步 `session.tool_mode`、`save_full` 覆盖 `tool_mode` 三个缺陷（含回归测试）。
- 2026-08-15：**修订二——档位补 confirm_apply**：edit/write 的 dry_run→pending_id→confirm_apply 确认闭环要求 confirm_apply 必须在档位内，否则模型被 dry_run 协议引导却无法落盘（其他模型尤甚）。A/B/C 三档统一补 `confirm_apply`（8/6/4 工具）；前端 ComboBox 标签同步为极限·8/极限·6/极限·4；edit 描述弱化 dry_run（直接调用即落盘，dry_run 仅预览确认用）。

- 2026-08-16：**修复——极简模式重启后被切回标准（tool_mode 锁死）**。
  - 根因：WinUI 冷启动首帧 `composer_bar` 的 tool_mode ComboBox 用空态渲染
    `SelectedIndex=0`，而 reactor 先绑 `SelectionChanged` 后应用
    `SelectedIndex`（首挂载 -1→0 必触发）；回调守卫 `next != rendered_tm`
    对空态失效（`"standard" != ""` 成立）→ 自动 `spawn_set_tool_mode("standard")`
    → daemon 把 meta.json 覆盖为 standard。permission_picker 曾有同型
    Bug#2 并已用 `rendered_pl==0` 闭合，tool_mode 没搬该防护。
  - 锁死修复：空态 = 未初始化 → 渲染 -1（无选中）+ 回调 `rendered_tm.is_empty()`
    一律跳过（CK-UI-GUARD）；`spawn_set_tool_mode` 白名单 + 幂等 + connect/action
    失败立即以 meta.json 权威回填（CK-UI-SEND）；`persist_tool_mode` 返回
    Result、写盘失败上抛 400、空串规范化为 standard（CK-PERSIST）；
    `apply_tool_mode` 未知值保持现状不回退全量（CK-APPLY）。
  - 检查点链：CK-UI-GUARD（前端渲染）→ CK-UI-SEND（前端发送）→ daemon
    service 值域校验 → CK-PERSIST（meta.json 落盘）→ Control SetToolMode →
    CK-APPLY（worker 应用）。任一环失败均可从日志定位；meta.json 始终是
    `sync_tool_mode_from_meta` 的唯一权威，前端重启 250ms 内回填。
  - 测试：deepx-session 8 passed；deepx-msglp lib 47 passed（新增 CK-APPLY
    未知值不覆盖断言 + 统一测试注册表修复 minimal:dsh flaky）；deepx-runtime
    104 passed；deepx-winui `cargo check` 通过。
