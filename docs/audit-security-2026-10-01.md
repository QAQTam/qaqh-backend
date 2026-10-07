# QAQH-Backend 白盒安全审计报告（2026-10-01）

> 审计方式：红队白盒攻击分析，**仅基于源码**（未读取本项目任何 docs 文件，避免被自述设计误导）。
> 范围：23 个 Rust crate + `webui/` 前端。方法：4 路并行深审（网络入口 / 命令执行链 / 策略权限 / webui 渲染）+ 独立复核 client SDK、依赖、脚本及各路关键断言。
> 威胁模型：本项目的核心攻击者是「被 prompt injection 的模型输出」+ 网页内容；其次为同机低权限进程、LAN 侧攻击者（`server` 模式）。

## 发现汇总（按严重度）

### High

#### H1. Windows/macOS 沙箱完全缺位，静默降级

- 位置：`crates/qaqh-sandbox/src/lib.rs:81-86`
- `wrap_command` 对非 Linux 平台直接返回 `SandboxBackend::None`，无 JobObject / 进程 token 降权，**非 fail-closed 且无用户可见警告**。而 `ToolCallContext.sandbox_spec` 一律声明 `workspace_write` + 网络 Deny（`crates/qaqh-policy/src/lib.rs:183-191`）——该声明在 Windows 上纯属装饰。
- 攻击链：一次 exec 审批被骗过（或 L4 `Unrestricted` 模式，`crates/qaqh-workspace/src/permission.rs:450-452` 全量 AutoApprove）→ 命令以用户完整权限运行：可写任意路径、可联网，与「沙箱内」预期完全不符。
- 影响面：项目主平台是 Windows（WinUI 壳），此缺位覆盖绝大多数用户。子代理不受影响（exec 在准入层即被拒）。
- 对照：Linux 侧实现到位——Landlock 默认禁写 + writable_roots 白名单、seccomp 封 `ptrace/unshare/mount/process_vm_*/kexec`（`crates/qaqh-sandbox/src/linux.rs:188-207`）、网络 Deny 封 socket，强制 `FullyEnforced` 否则失败。

#### H2. 工作区 skill 写入零审批 → 「权威指令」持久注入链

- 位置：`crates/qaqh-skills/src/lib.rs:179-191`（发现目录含工作区 `.qaqh/skills`）；`crates/qaqh-workspace/src/permission.rs:472-476`（L3 工作区内 Write → AutoApprove）；`crates/qaqh-skills/src/runtime.rs:566-570`（envelope 声明 "This is the complete authoritative active skill set. It replaces all older skill instructions"）
- 攻击链：被注入的模型在默认档 L3（`crates/qaqh-config/src/config.rs:643`）下调用 write 工具写 `.qaqh/skills/helper/SKILL.md` —— 工作区内写自动放行，无弹窗。下一用户回合 `refresh_catalog`（`runtime.rs:493-532`）自动发现；模型再调 `skills(action=activate)`，正文以**系统级权威指令**身份注入且「替换所有旧 skill 指令」。文件留在仓库 → **跨会话持久生效**。
- 放大器：`explicit_mentions`（`lib.rs:682-714`）对 `$name` 触发 `source="user"`——用户粘贴含 `$helper` 的网页文本即伪造「用户主动请求」来源，模型忽略两个 lap 后宿主还会**强制激活**（`runtime.rs:134-159`）。
- 边界：不直接提权——skill 的 `allowed_tools` 仍与策略求交（`lib.rs:57-58`）。但构成指令洗白 + 持久驻留。

#### H3. `server` 子命令默认绑 `0.0.0.0:64413`，明文 HTTP 传 Bearer token

- 位置：`crates/qaqh-daemon/src/server.rs:73-80`（`parse()` 默认 `Ipv4Addr::UNSPECIFIED` + 固定端口 64413）、`:156`（bind）、`:167`（`http://` 明文端点）
- 攻击链：LAN 内被动嗅探 `Authorization: Bearer ...` 头 → 截获 256-bit token → 完全接管 agent：读任意会话文件、驱动工具执行、`config.set_permission_level` 直接设 L4（`crates/qaqh-runtime/src/service.rs:545-559`）。
- 缓解：默认随机 token 不可暴破；discovery 文件限权；无 CORS 层故浏览器跨域无法带自定义 Authorization 头。无任何 TLS 配置路径，代码自述「临时跨端模式，不做任何安全加固」（`server.rs:49`）。
- 边界：普通 `run` 模式默认 loopback + 随机端口，不受影响。

### Medium

#### M1. exec 子进程完整继承 daemon 环境，denial 日志未脱敏

- 位置：`crates/qaqh-workspace/src/exec/direct.rs:124-126`（仅 `cmd.envs(env)` 追加，无 `env_clear`）
- 一次批准无害命令 `Get-ChildItem Env:` 即可把 daemon 进程全部环境变量回传给模型（若 API key 以环境变量注入则直接泄漏）。
- 叠加：`record_denial_if_any` 把命令输出前 512 字符**原样**写入 warn 日志，未经 `redact_secrets`（`crates/qaqh-sandbox/src/lib.rs:307-319`）。

#### M2. 裸 shell 名 + Windows 当前目录搜索 → 工作区二进制投放

- 位置：`crates/qaqh-workspace/src/exec/shell.rs:208-245`（返回裸名 `"pwsh"`/`"cmd"`）；`exec/direct.rs:95`（`Command::new(argv[0])`）；cwd 默认为模型可写的工作区（`exec/handler.rs:223-230`）
- Windows `CreateProcess` 的可执行搜索序含**当前目录**：模型先在工作区投放改造过的 `pwsh.exe`（可从 System32 拷贝），之后每次 exec 实际启动的都是投放的 shell——绕过壳探测（EncodedCommand 等）的全部语义，且与 H1 叠加即无任何隔离。
- 同类：MCP/LSP `Command::new(&cfg.command)`（`crates/qaqh-mcp/src/adapter.rs:114`、`crates/qaqh-lsp/src/adapter.rs:60`），`npx`/`node` 类配置可被 PATH 劫持。Linux 侧 helper 用 `execvp` 但处于 Landlock 约束下，不构成逃逸。

#### M3. 沙箱内全盘可读（Linux）

- 位置：`crates/qaqh-sandbox/src/linux.rs:109-145`（Landlock 仅 `AccessFs::from_write`）；bwrap `--ro-bind / /`（`lib.rs:212`）
- 已批准的沙箱内命令 `cat ~/.ssh/id_rsa`、读 `secrets.toml` 均合法；结果写入工作区可写根（合法），再由主代理的联网工具外发。网络 Deny 只封直接出站，不封「两跳」外带。

#### M4. L4 下 `~/.qaqh/config.toml` 不受敏感路径保护，自提权可热生效

- 位置：`crates/qaqh-workspace/src/permission.rs:348-376`（敏感名单只含 sessions/messages.jsonl/secrets.toml 等，**不含 config.toml**）；`crates/qaqh-config/src/watch.rs:76-84`（mtime 轮询自动 `reload_from_disk` 发布）
- L4 下模型重写用户目录 config.toml：固化 `permission_level = 4`；改 `base_url`（用户值无条件优先，`config.rs:793-797`）把后续 LLM 请求（携带 api_key）导向攻击者端点。写盘后**免重启热生效**。
- 边界：已确认配置只从用户目录读取（`crates/qaqh-config/src/platform.rs:333-335`），工作区无配置文件 → **L1–L3 不存在此路径**；L4 本身是显式危险模式，故定为 Medium。

#### M5. daemon 可执行文件解析含 cwd 与 PATH → 本地二进制劫持

- 位置：`crates/qaqh-client/src/discovery.rs:164-202`（`daemon_executable()`）
- 候选序：`QAQH_BACKEND_ROOT/target/debug` → **当前工作目录** `target/debug` → exe 同目录 → **裸名 PATH 查找**。用户在含 `target/debug/qaqh-daemon(.exe)` 的目录（如克隆的恶意仓库）启动客户端，即可拉起伪装 daemon——获得 token、以用户权限常驻。最后一级 PATH 查找同样可被 PATH 劫持。

#### M6. webui：模型输出可嵌任意外部链接（钓鱼面）

- 位置：`webui/src/lib/safe-url.ts:30-32` + `webui/src/lib/streaming-md.ts:57-62`
- `safeMarkdownHref` 仅校验协议 http/https 并拒绝 userinfo，不做域名限制。模型输出嵌入 `[登录网关](https://evil.example/login)` 渲染为可点击链接；`rel="noopener noreferrer"` + `referrerpolicy="no-referrer"` 已防反向 tabnabbing，但仿冒页钓鱼未防（CSP 对新窗口不生效）。
- 相关（Low）：`safeMarkdownImageSrc` 限同源，但 `![x](/__gateway/某GET路由)` 可作探针触发网关 GET 副作用 / 探测路由存在性（无外带数据通道）。

#### M7. 信任目录进程级全局、永久持久化

- 位置：`crates/qaqh-workspace/src/authorization.rs:466-485`（`TrustedFolderSet::load("")`）；`permission.rs:546-607`（落盘 `sessions/trusted_folders.json`）
- 一次「记住此文件夹」后，整个子树对**所有未来会话**的跨工作区写永久自动放行（`permission.rs:494-503` 子树匹配），无过期、无会话隔离。
- 缓解：该文件位于 sessions 目录下，直接读写被 `is_sensitive_session_path` 强制弹窗，模型无法静默篡改。

### Low / Info

| # | 发现 | 位置 | 说明 |
|---|------|------|------|
| L1 | 合规过滤白名单前缀一票通行 | `crates/qaqh-policy/src/input_guard.rs:25,34-41` | `research:`/`academic:`/`crypto:` 前缀完全绕过全部封锁词；且守卫只覆盖用户输入，网页内容与模型输出不过滤 |
| L2 | 非 Windows secrets.toml 明文 | `crates/qaqh-config/src/secrets.rs:491-501` | TODO keyring；Windows 走 DPAPI CurrentUser（同用户进程可解密，属 DPAPI 固有边界） |
| L3 | ProcessRegistry 无属主校验 | `crates/qaqh-workspace/src/process_registry.rs:301`；`process_inspect.rs:89-96` | `process check/wait` 属 Read 类且无路径参数 → 子代理沙箱自动放行，可枚举其他会话后台进程的捕获输出（跨代理信息泄露）；`kill` 属 Exec 仍需审批 |
| L4 | webui 生产构建常驻调试钩子 | `webui/src/state.ts:563-574` | `window.__webuiDebug` 暴露内部函数，无环境开关；XSS 发生时是便利原语 |
| L5 | pwsh `-CommandWithArgs` 参数二次解析 | `exec/shell.rs:300-318` | 含空格/`-` 前缀的 args 被 pwsh 重新解析——数据混淆非脚本注入（模板与数据分离正确） |
| L6 | 多行命令日志注入 | `exec/handler.rs:385` | `log::info!` 原样输出命令片段，可伪造日志行 |
| L7 | SSE 解码无帧长上限 | `crates/qaqh-client/src/sse_decoder.rs` | 恶意无换行流可致客户端内存增长；仅本地可信 daemon 流，低危 |
| I1 | daemon HTTP 面无 Host/Origin 校验 | `crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:138-192` | 鉴权走 Authorization 头（非浏览器 ambient 凭据）→ CSRF/DNS rebinding 无凭据可劫持，当前不可利用；`/health` 唯一免鉴权端点不泄敏感信息 |
| I2 | 网关 nonce 赎回不校验颁发 IP | `crates/qaqh-webui-gateway/src/session.rs:120-147` | 256-bit 随机 + 60s TTL + 单次使用；网关只绑 127.0.0.1，无意义 |
| I3 | webui 依赖预发布版本 | `webui/package.json:14-24` | solid-js RC、vite 插件 next——无已知 CVE，成熟度风险 |
| I4 | API key 可能渲染进 DOM placeholder | `webui/src/components/Settings.tsx:216` | 前端写路径有 `****` 守卫，读路径依赖后端掩码（后端确有掩码，当前无泄漏） |

## 确认安全的方面（要点）

- **渲染管线（webui）**：全 src 无 `innerHTML`/`insertAdjacentHTML`/`eval`/`new Function` 汇，且有静态测试断言此约束（`webui/tests/rendering-safety.test.ts:6-15`）；流式 mXSS 不适用（streaming-markdown 走 token→DOM API，从不序列化再 re-parse）；CSP `default-src 'none'; script-src 'self'`（`crates/qaqh-webui-gateway/src/lib.rs:1366`）+ COOP/CORP/nosniff；URL scheme 校验先拒控制字符/空白再 WHATWG 解析后白名单（`safe-url.ts:8-23`），`javascript:`/`data:`/`vbscript:` 大小写与实体变体全被中和。
- **令牌纪律**：daemon 侧 SHA-256 摘要 + `subtle::ConstantTimeEq` 常时比较（`axum_server/axum_impl/auth.rs:14-17`）；网关 CSRF 同为 `ct_eq`；Bearer 走 Authorization 头不走 URL；token 每次 daemon 启动随机化；discovery 文件 0600/icacls 限权。
- **路径边界**：`resolve_target_path` 对最近存在祖先 canonicalize + 词法规范化——`..` 穿越、symlink/junction、8.3 短名、大小写折叠全部消解且 fail-closed，有回归测试（`permission.rs:786`）；`path_within_dir` 分量级前缀比较防 `D:\shared-other` 误判；资产服务组件级校验拒 `..`/隐藏文件/`%2e%2e`（`webui-gateway/lib.rs:349-367,1553`）；`parse_loopback_endpoint` 拒绝非 loopback/userinfo/path/query（`lib.rs:197-241`）——SSRF 面收敛。
- **shell argv 构造**：POSIX `sh -c script _ arg...` 位置参数模板（`shell.rs:286-298`）；pwsh 默认 `-EncodedCommand` Base64(UTF-16LE)（`shell.rs:320,352-358`）规避 Win32 引号地狱；cmd 显式拒绝 `args`（`handler.rs:186-193`）——**无字符串拼接注入**。
- **secrets**：API key 永不落 config.toml（仅 `set` 标记，`config.rs:1086-1091`）；DTO `****` 掩码；`${secret:}` 只进子进程 env 不回存，load 时 fail-fast 校验；`redact_secrets` 按长度降序防前缀残片（`crates/qaqh-mcp/src/sanitize.rs:11`）；审批对话框刻意省略 exec 的 env。
- **网关命令白名单**：`sanitize_command` 仅放行 SendMessage（强制 `as_system=false`）与 Cancel；服务白名单不含 `config.save`/`workspace.delete`；envelope 身份字段网关覆写不采信浏览器值（`webui-gateway/lib.rs:811-814`）。
- **子代理沙箱**：Exec/Net/跨工作区一律 `Admission::Denied`，MCP 工具 host-only（`authorization.rs:360-443`）；MCP 动态工具 stdio=Exec/http=Net，L1-3 全部进审批。
- **会话与审计**：会话日志反序列化 fail-closed（torn tail/fact_seq 断档/identity mismatch/poison marker 均报错不 panic，`crates/qaqh-session/src/canonical/reader.rs:196-263`）；审计账本双写 + SHA-256 哈希链 + quarantine 硬闸，参数只落哈希不落明文。
- **MCP 导入**：项目级 `.mcp.json` 需逐 server 审批回调（`crates/qaqh-mcp/src/mcp_import.rs:387-392`）。
- **权限级别**：越界值 fail-closed 降级 MaxLockdown（`config.rs:914-924`，含全值域回归测试）。
- **依赖**：axum 0.8.9 / hyper 1.11.1 / rustls 0.23.45 / url 2.5.8 / idna 1.1.0 / tokio 1.53.1——均无已知重大 CVE 版本；`clippy unwrap_used=deny` 降低 panic-DoS 面；16MB body 上限 + 并发上限。
- **无硬编码凭据**；无 eval/动态执行；`qaqh-gate` 出站 URL 全部来自用户本地配置，非远端可控。

## 修复优先级建议

1. **H1 + M2（Windows 侧，最高优先）**
   - 非 Linux 平台沙箱缺失至少 fail-closed 或启动/审批时显式警告「当前平台无沙箱」；
   - exec shell 解析改绝对路径（`where.exe`/固定 System32 路径），杜绝当前目录搜索；
   - MCP/LSP 命令解析同样收敛为绝对路径。
2. **H2**：`.qaqh/skills` 写入或首次 skill activate 升级为 AskUser；`explicit_mentions` 的 `$name` 来源不应标 `source=user`；考虑 skill 文件带 hash 指纹，变更后重新确认。
3. **H3**：`server` 模式默认改 loopback；非 loopback 绑定强制要求显式 `--token` + 交互式二次确认；中期上 TLS，或至少在文档与启动横幅中标注「不可用于不可信网络」。
4. **M1**：exec 子进程 `env_clear` + 最小白名单透传（`PATH`/`SYSTEMROOT`/`TEMP` 等）；denial 日志过 `redact_secrets`。
5. **M4**：把用户目录 `config.toml` 加入敏感路径名单（即便 L4 也强制弹窗）。
6. **M3**：考虑 Landlock 增加读限制选项（`ReadFile`/`ReadDir` 按白名单），或至少在文档中明确「沙箱不防读」。
7. **M5**：`daemon_executable()` 移除 cwd 候选，PATH 查找仅在显式 opt-in。

## 跟进:webui Tauri 化后的信任边界变化(2026-10-02)

按 `docs/archive/plan-webui-tauri.md` 完成 A→D 阶段后,本报告与 webui 相关的结论按
下述口径更新:

**已移除的面(随 `qaqh-webui-gateway` 删除)**

- gateway 的 nonce 引导 / HttpOnly 会话 / CSRF / origin 白名单 / 限流全部退役
  ——这些机制防御的「不可信浏览器跨站与脚本面」在 Tauri 壳模型下不存在;
- 网关命令白名单(`sanitize_command`)退役;IPC 面改为 `invoke_handler`
  类型化注册即白名单,`service_rpc` 保留与原网关一致的 19 方法白名单,
  写类动作(`config.save`/`workspace.*` 写法)不在其中;
- 静态资源内嵌(`RustEmbed` + 扩展名白名单)退役,由 `frontendDist` + Tauri
  自定义协议接管。

**新信任边界**

- **token 仅宿主**:daemon bearer token / lease id 只存在于 Rust 宿主进程
  (`qaqh-webui-app` + `qaqh-client`);webview 可达面 = 类型化 IPC + 宿主转发
  事件,均不携带凭据。报告「确认安全」中的 token 纪律条目(daemon 侧常时
  比较、0600 discovery)不变。
- **审批防御纵深保留并前移**:gateway `approval.rs` 移植为宿主
  `challenge.rs`——webview 只见不透明 challenge id(64 hex,TTL 5min,
  一次性消费 + active-seed scope 校验),canonical `call_*`/`int_*` id 不出
  宿主;审批提交仍不受 driver 门控(daemon 侧语义不变)。
- **CSP 单点化**:构建期不再注入 meta CSP;`tauri.conf.json >
  app.security.csp`(`default-src 'self'` + `ipc:`)单点接管,消除双重 CSP
  相互削弱。渲染管线约束(无 innerHTML/eval、DOMPurify、textContent 构建)
  不变,仍由 `webui/tests/` 断言。
- **外链收敛**:webview 不导航外站,http(s) 外链经宿主 `open_external`
  (scheme 白名单 + 系统 opener)。
- **新增建议**:宿主侧 `challenge.rs` 与 `commands.rs` 是新的安全敏感面,
  后续变更需与原 gateway 同等强度审查(TTL/一次性消费/scope 语义不可放宽);
  `conn://liveness` 节流与 `projection://event` 直发需在上线阶段做一次高
  吞吐回合的背压观察(plan B4)。
