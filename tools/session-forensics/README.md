# session-forensics — 压缩上下文后的精准取证

> 只读的会话取证工具。回答「压缩之后我到底忘了什么」：用户原话是什么、
> 改过哪些文件、做过什么决定、todo 还剩什么——**逐条带 `msg_id` 出处**。
>
> 面向所有平台（纯 Python 标准库，无第三方依赖；已在 Python 3.14 验证）。

## 0. 30 秒上手

```bash
# 压缩后恢复：一次拿到最该记住的事实（用户原话 + todo + 最近工具 + 触碰文件）
python3 tools/session-forensics/session_forensics.py evidence

# 用户原话逐条列（压缩后最易失真的部分）
python3 tools/session-forensics/session_forensics.py users

# 定位「某件事发生在哪」：正则检索，带 msg_id 窗口
python3 tools/session-forensics/session_forensics.py search 11155 --role user
python3 tools/session-forensics/session_forensics.py search "reasoning_content" --since 900 --max 20

# 完整还原某条消息（含 reasoning / tool_use / tool_result）
python3 tools/session-forensics/session_forensics.py show 600

# 摸清会话面
python3 tools/session-forensics/session_forensics.py sessions
python3 tools/session-forensics/session_forensics.py info
python3 tools/session-forensics/session_forensics.py todo
python3 tools/session-forensics/session_forensics.py files
python3 tools/session-forensics/session_forensics.py timeline | tail -40
```

所有子命令都接受 `--session <seed>`（默认取 `.active_session`，没有则取最近更新
的会话）和 `--json`（机器可读）。`--root` 可指向 data_root 或 `sessions/` 目录。

## 1. 为什么需要它

上下文压缩（compaction）会把早期对话折叠成摘要，**摘要必然失真**：用户的原话被
转述、改过的文件被概括、决定的理由被抹平。但磁盘上的原始日志**从未被压缩**：

```
{data_root}/sessions/{seed}/
    meta.json            会话元信息（模型/effort/cwd/token 统计）
    messages.jsonl       追加写的权威消息流（一行一条 Message）★ 唯一真源
    messages.wal         L2 预写日志（未 drain 的 persist op）
    compact-context.json 当前压缩检查点（摘要 + 保留的消息）
    todo.json            任务计划
    tool_outbox.wal      工具调用回执（call_id/name/status/ts）
    code_stats.jsonl     文件改动行数统计
```

本工具**只读**这些文件，因此：不写盘、不改会话、不依赖 daemon 在跑。
`messages.jsonl` 是 append-only 的，`msg_id` 单调递增且可作引用锚点——
取证结论请一律引用 `msg_id`，而不是"摘要里说"。

## 2. 数据根自动定位

镜像 Rust 侧 `qaqh_types::platform::data_dir()` 的解析顺序：

1. `$QAQH_DATA_DIR`（完整 data root）
2. Windows：`%USERPROFILE%\.qaqh`
3. Unix：`$XDG_CONFIG_HOME/qaqh` 或 `$HOME/.config/qaqh`

若 data_root 下有 `.qaqh-data-root.json`（多实例/自定义根标记），以标记里的
`canonicalRoot` 为准。

## 3. 子命令速查

| 命令 | 回答什么问题 |
|---|---|
| `sessions` | 本机有哪些会话？各自的模型/更新时间/cwd |
| `info` | 这条会话的元信息 + **文件健康**（撕裂行、未 drain 的 WAL op） |
| `users` | 用户原话逐条（带 `msg_id`、字符数）——**压缩后优先看这个** |
| `search <re>` | 在 text/reasoning 里正则检索，支持 `--role/--since/--until/--max/--context` |
| `tools` | 工具调用清单（`--name` 过滤、`--last N` 取尾部） |
| `files` | 工具触碰过的路径（按引用次数排序）+ `code_stats.jsonl` 增删行数 |
| `show <msg_id>` | 完整打印一条消息的所有 content block |
| `todo` | 当前任务计划（含 completed 的 evidence） |
| `wal` | `messages.wal` 里未 drain 的 op（内存态领先盘面的信号） |
| `timeline` | 按 `msg_id` 折叠的紧凑时间线（定位"某件事在哪"） |
| `evidence` | **取证包**：用户原话 + todo + 最近工具 + 触碰文件 + 告警，一屏 |
| `selftest` | 内置自检（临时目录造样例，不触碰真实会话） |

## 4. 设计取舍

- **只读、无副作用**：不写任何文件（`selftest` 也只写临时目录），可对生产会话用。
- **不猜 schema**：字段名对齐 `qaqh-types/src/message.rs`（`Message`/`ContentBlock`）
  与 `qaqh-types/src/tool_result.rs`（`ToolResult.status/summary/model.text`）。
- **容忍半行**：追加写文件可能有撕裂的末行，`load_jsonl` 计入 `torn` 并跳过，
  在 `info`/`evidence` 里作为告警显示，绝不静默吞掉。
- **工具输入摘要化**：`exec` 展示 `argv`、`apply_patch` 提取 `*** Update File:` 路径、
  `write/edit` 展示 `path`——避免把整份 patch 糊到屏幕上。
- **不解析模型语义**：只做结构提取（谁说了什么、调了什么工具、碰了哪些文件），
  "这意味着什么"留给读的人。

## 5. 局限

- `files` 的路径收集是**启发式**（键名含 `path`/`file`），不是 AST 级分析；
  正则写在 `collect_paths` 里，误报时改那里。
- 压缩检查点只报告元信息（`checkpoint_id`/`parent`/`archive_message_count`），
  不展开摘要正文——摘要本身会失真，取证应回到 `messages.jsonl`。
- 若会话正在被 daemon 写入，读到的可能是瞬时状态；`wal` 有未 drain op 时以
  `messages.jsonl` 为准（它就是 append-only 真源）。
