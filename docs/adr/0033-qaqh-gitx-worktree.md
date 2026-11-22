# ADR-0033 qaqh-gitx：工作区 git 消费与 worktree 会话隔离

状态: `已采纳` · 决策日期 2026-11-22 · 决策人 仓库所有者（明确指示：本次非 LTS，可开始设计前端接口并让
前端 electron 等消费——ADR-0031 词汇下发版可破坏性演进）· 实施: `crates/qaqh-gitx` +
`qaqh-ringing-contract::gitx` 本批同落

## 背景（每条均已核实，附出处）

1. **前端需要消费 git 状态**：契约唯一来源是 `qaqh-ringing-contract`（crate 头注释：
   "本 crate 是客户端与 daemon 的唯一契约来源"），经 `ts(export, export_to = "qaqh/")` 由
   `tools/qaqh-contract-export` 产出 TS/C# 三端产物（ADR-0002/0006/0008）。git 域目前没有契约面。
2. **并发会话同写一个工作区互相干扰**：多会话共享 `ExecutionContext.workspace`（单值线程本地），
   同仓多会话的写路径、stale-file ledger、git 操作全部落在同一棵树上。
3. **业界先例（2026-11-22 一手抓取源码核实）**：
   - openai/codex `codex-rs/worktree/src/lib.rs`：`ManagedWorktree{root,cwd,source_root,source_cwd,
     head_sha,branch}`，注释明言 cwd 用于启动 thread；`paths.rs` bucket = UUID4 前 4 hex；
     `metadata.rs` 用 `NamedTempFile.persist_noclobber` 写 `codex-thread.json` 做原子 owner 绑定；
     `settings.rs` 池根默认 `codex_home/worktrees`（**仓库外**）；worktree 生命周期走 **git CLI**。
   - xai-org/grok-build `xai-fast-worktree`（Grove）：`git worktree add --no-checkout` + 并行 CoW
     克隆 + BTRFS/projfs/NFS 投影策略 + SQLite 注册表 + auto-GC；数据目录在 `$XDG_DATA_HOME/grove`
     等（**仓库外**）。两家的 worktree 均不在项目目录内。
   - 两家均**不做**提示词路径引导与工具层静默写重定向；隔离发生在会话/thread bootstrap 或
     显式会话级绑定。
4. **qaqh 已有的插槽（一手核实）**：
   - `qaqh-permission::ExecutionContext{session_id, workspace}` 线程本地 + actor 上下文**不切进程
     cwd**（`workspace.rs::set_process_workspace` 注释：进程 cwd 是共享资源，actor 模式下路径解析
     不依赖进程 cwd）——会话级重绑定的现成机制。
   - `qaqh-sandbox::wrap_command` 每次 exec 前 `canonicalize_spec` 重算 `SandboxSpec.writable_roots`
     （`lib.rs:465`）——沙箱同步是**派生**不是变异，天然支持绑定切换。
   - `sbx-win` 已有 WindowsRedirect（ProjFS）平面（`qaqh-sandbox/src/lib.rs:157-196` 注释），
     将来可承载快速物化。
5. **gix 0.90**（crates.io 2026-10-22，MSRV 1.88）提供 status/dirwalk/excludes/attributes 等 feature；
   其 `worktree-mutation` 只覆盖工作区文件增删，**不含** `git worktree add` 的 linked-worktree 管理
   文件注册——codex 与 grok 的 worktree 生命周期因此都走 git CLI。gix 消费 status 有先例
   （codex 依赖 gix 0.89；grok 有专门 `xai-gix-status` crate）。

## 决策

1. **crate 分工**：新引擎 crate `crates/qaqh-gitx`（git 操作、注册表、owner 绑定、快照）；
   wire 类型进 `qaqh-ringing-contract::gitx`（schema 常量 `qaqh.ringing.gitx/v1`，形状锁测试同权，
   对齐 `TEAM_SCHEMA` 惯例）。依赖方向：gitx → contract，契约不依赖实现。
2. **路径布局**：worktree 建在 `{source_root}/.qaqh/worktree/{slug}-{hash4}`（产品诉求：前端与
   用户在项目内可见）。防侵蚀措施：
   - 目录名**不用**原始 branch 名：slug 化（Windows 非法字符/保留名/尾部点空格处理）+ uuid 前 4 hex
     防碰撞（codex 同款手段）；
   - 创建时确保 `{source_root}/.qaqh/.gitignore` 含 `worktree/`，git 状态/dirwalk/journal 全链路
     跳过嵌套树；
   - 注册表 `registry.json` 同目录存放，原子写（tempfile + persist）。
3. **绑定协议（会话级，非 turn 级）**：
   - `WorktreeBinding{worktree_root, source_root, branch, head_sha, owner_session}`；
   - 绑定 = actor 上下文 `workspace := worktree_root`（代码路径）；`.qaqh` 工件（sessions/tasks/
     trash/memory）**继续锚定 source_root**——双锚点对齐 codex 的 source_root/cwd 分离，避免会话
     资产落进 worktree 差异里；
   - 所有权：registry 内 compare-and-set + 原子落盘，一个 worktree 同时至多一个会话（v1 单 daemon
     进程内强一致；跨进程粒度留待 v2 采纳 codex 的 per-worktree gitdir 文件形态）；
   - 切换仅在会话空闲时执行；模型经工具调用触发并收到环境块更新，**不做**静默写重定向、
     **不靠**提示词引导路径。
4. **沙箱同步 = 派生而非变异**：绑定后 `SandboxSpec.workspace_root`/`writable_roots` 在 exec 时从
   绑定解析（`wrap_command` 现流程零改动）；否决 grok 式"创建时把路径写进 allow 档案"的持久放宽
   ——与 fail-closed 审批哲学（`qaqh-sandbox` lib.rs 头部审计注释 H1）冲突。主工作区在绑定会话中
   降级为只读 + 跨区写走 `trust_folder` 一次性信任。
5. **git 操作分层**：linked-worktree 生命周期走 git CLI（对齐 codex/grok，gix 不覆盖管理面）；
   status/head/branch v1 走 CLI porcelain，v2 迁 gix 0.90（`status`/`dirwalk`/`excludes` features，
   版本随 ADR-0025 精确锁纪律处理）。CoW/projfs 快速物化不在本 ADR 范围（v3 评估，承载面
   sbx-win Redirect）。
6. **模型暴露面**：环境块自动告知（workspace 行 = worktree 路径 + 源仓库）；工具面
   `worktree_list/create/enter/remove` + `git_snapshot`；未注册 worktree（绕过工具手工创建）以
   发现式枚举标注 `unmanaged`，不享受自动放行。
7. **前端接口（非 LTS，可破坏性演进）**：读模型 `WorktreeView`/`GitSnapshot`（snake_case，对齐
   ringing 契约惯例；与 `config-api` 的 camelCase K2 域不同域不混用）+ 命令参数
   `WorktreeCreateParams`/`WorktreeRemoveParams`；ts export 进契约 zip，electron/web/winui 同源消费。

## 后果

- 正面：并发会话隔离成型；前端 git 消费有单一契约面；沙箱无热变异路径，审计面不变。
- 风险：嵌套 worktree 在仓库内的清理/扫描面（excludes 纪律是硬前提，创建函数强制维护）；
  Windows 路径长度（slug 截断 + 短 hash）；gix 迁移时 0.x API 漂移（v2 用精确锁缓解）。
- 迁移成本：无——新 crate + additive 契约，非 LTS 线无冻结约束。
