# Session 修改日志（SMJ）设计 —— 事故复盘与恢复机制

> 状态：基础版已落地（2026-08-17）
> 背景：前端仓库误删事故。未提交修改仅存在于被删仓库工作树，git 历史、
> reflog、远端 dangling 对象均不可恢复；最终靠主仓库落后快照 + 其它 agent
> 的 session 记录（messages.jsonl 中的工具调用参数）找回大部分改动。

---

## 1. 事故复盘：丢失链条

| 环节 | 为什么 git 救不了 |
|---|---|
| 5 个 winui 文件的未提交修改 | 从未进入任何 commit / reflog / git 对象库 |
| 旧前端仓库整体被删 | reflog 随 `.git` 目录一起消失 |
| 远端被空仓 force push | GitHub 在短时间内 GC 掉旧对象（按 SHA 查询 422） |
| 回收站 | shift-delete，为空 |

**实际救命的两样资产**：
1. 主仓库历史里落后一个版本的快照（96 个前端文件）；
2. `~/.deepx/sessions/d0edd1e0/messages.jsonl` 中另一 agent 的工具调用参数
   （找回 BUG-013/014 的完整 patch）。

**暴露的缺口**：
- session 记录依赖 `messages.jsonl` 存活，且只有会话被完整落盘才有；
- 修复 BUG-003 的会话记录已不存在，只能按 bugfix 档案人工重写；
- 工具参数里有代码，但无文件级索引，检索靠全文 grep；
- 没有"任意文件在任意时点的最终内容快照"。

---

## 2. 结论

**值得做，且做成"编辑事实日志"（event-sourced edit journal），不是
"又一个聊天备份"。** 它与重构路线图刀 2（一条日志 + 投影）同构——
本质是 workspace 侧的编辑事件溯源，并可同时服务：事故恢复、undo/回放、
审计。

---

## 3. SMJ（Session Modification Journal）设计

### 3.1 目录结构

```
~/.deepx/journal/
├── steps.jsonl              # append-only 步进索引（小）
└── blobs/<sha256>           # content-addressed 存储（大）
```

### 3.2 步进记录（step）

```jsonc
{
  "seq": 1024,                     // 全局递增
  "session": "d0edd1e0",           // 会话 seed
  "tool_use_id": "call_01_xxx",    // 与 messages.jsonl 互链
  "ts": 1755300000,                // epoch 秒
  "tool": "edit",                  // write/edit/apply_patch/delete/file_mutate
  "file": "apps/winui/src/bridge.rs",
  "op": "replace",                 // 工具细粒度操作
  "before_sha": "sha256:…",
  "after_sha": "sha256:…",
  "patch_sha": "sha256:…",         // blobs 中的 patch 内容
  "result": "ok",                  // ok / failed / reverted
  "git_before": "HEAD"             // 最近检查点（commit sha）
}
```

- `patch_sha` 指向 `blobs/` 中的 diff（或工具的 old/new 原始参数）；
- 全文快照策略：`before/after` 内容 ≤ 1MB 存全文，否则仅存 patch；
- 写入顺序：step 先落盘（fsync），再回执工具成功——崩溃最多丢最后一步。

### 3.3 记录点

workspace 工具层所有写路径收口处（预计 4-6 个文件）：
`write` / `edit` / `apply_patch` / `delete`（含 `file_mutate` 底层）。
复用现有 `audit.rs`、`pending.rs`、`file_cache.rs`。daemon 已常驻，
无需新后台进程。

### 3.4 查询与恢复接口

```
journal query  --session <id> [--file <path>] [--since <ts>]
journal replay <file> [--at <seq>] [--out <dir>]     # 沿步进重放
journal export --session <id> [--format patches]     # 导出可再应用的补丁集
```

作为 workspace 工具暴露给模型，误删后模型可直接调取步进恢复。

### 3.5 设计原则

1. **append-only**：不修改历史，只追加；压缩通过检查点折叠。
2. **未提交也要记**：这是相对 git 的增量价值。
3. **可修剪**：git commit 成功后以 `git_sha` 为检查点，旧步进折叠为
   检查点前快照，控制磁盘增长。
4. **与 messages.jsonl 互链**：step 存 `tool_use_id`，对话上下文与事实
   步进互相检索（本次事故的检索方式过于原始）。
5. **独立于仓库 .git**：journal 位于用户数据目录，仓库删除不丢。
6. **不阻塞主流程**：写失败降级为日志告警（工具结果仍返回），但默认
   fsync 保证恢复能力。

---

## 4. 磁盘与成本估算

| 项 | 估算 |
|---|---|
| 写路径接入点 | 4-6 个文件 |
| 新增模块 | journal（append/index/query/replay/compact）300-500 行 Rust |
| 每步磁盘 | patch 数 KB；全文快照按阈值（≤1MB）——实际增长可忽略 |
| 接口 | CLI + workspace 工具 schema 暴露 |

---

## 5. 落地路线

- **时机**：作为刀 2（持久化收敛）的前置小步，与"一条日志 + 投影"
  方向一致；不碰协议、不碰前端。
- **验收**：
  1. 一次模拟误删（删除仓库 + 远端覆盖）后，能按 session 导出该会话
     全部编辑步进并重放恢复文件；
  2. `journal replay` 恢复的文件与误删前内容逐字节一致；
  3. 模型能通过工具查询/恢复，无需人工 grep 聊天记录。

---

## 6. 未解决问题（后续再议）

- journal 与 audit 的关系：合并为同一事件流，还是 audit 作为 journal 投影；
- 多机/多 home 场景（WSL 与 Windows 共享 ~/.deepx）的 journal 归属；
- 与刀 2 事件溯源日志的最终统一格式。

---

## 落地记录（2026-08-17）

- 新增 `qaqh-workspace::journal` 模块：`steps.jsonl` + `blobs/<sha256>`，支持
  `record_change` / `query` / `replay_file` / `replay_to_path` / CLI。
- 接入写路径：`write` / `edit` / `apply_patch` / `delete` / `copy_range` /
  `web_fetch(output=...)`，成功落盘后追加步进，失败不阻塞工具结果。
- 新增 workspace 工具 `journal`（action=query/export/replay），并支持
  `qaqh-workspace journal query|replay|export` CLI。
- `journal export --format patches` 可导出 unified diff 补丁集。
- 基础验收：`cargo test -p qaqh-workspace` 全绿；replay 可恢复小文件内容。
