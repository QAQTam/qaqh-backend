#!/usr/bin/env python3
"""session-forensics — 从 QAQ-Harness 会话原始日志里精准取证。

背景：上下文压缩（compaction）之后，模型只拿得到一份摘要，原始事实
（用户到底怎么说的、改过哪些文件、做过什么决定、todo 剩什么）都会失真。
但**磁盘上的原始日志从未被压缩**：

    {data_root}/sessions/{seed}/
        meta.json           会话元信息（模型/effort/cwd/token 统计）
        messages.jsonl      追加写的权威消息流（一行一条 Message）
        messages.wal        L2 预写日志（未 drain 的 persist op）
        todo.json           任务计划
        tool_outbox.wal     工具调用回执（call_id/name/status/ts）
        code_stats.jsonl    文件改动行数统计

压缩事实（v2「route 1」体系）：压缩摘要直接以 `[Compacted N turns]` 合成
消息写入 messages.jsonl（唯一真源，route 1 不再生成第二个 compact 真源文件）；
meta.json 的 `compact_skip` / `compact_covered_through_msg_id` 是压缩水位。

本工具**只读**这些文件，输出可引用的取证结论（带 msg_id 出处），
供模型在压缩后重建事实。不写盘、不改会话、不依赖 daemon。

用法（30 秒上手）：

    python3 tools/session-forensics/session_forensics.py evidence
    python3 tools/session-forensics/session_forensics.py users
    python3 tools/session-forensics/session_forensics.py search 11155 --role user
    python3 tools/session-forensics/session_forensics.py show 600
    python3 tools/session-forensics/session_forensics.py sessions --json

默认自动定位：`$QAQH_DATA_DIR` → `$XDG_CONFIG_HOME/qaqh` → `~/.config/qaqh`；
会话取 `.active_session`，没有则取最近更新的一条。
"""

from __future__ import annotations

import argparse
import json
import os
import re
import sys
from datetime import datetime, timedelta
from pathlib import Path

WAL_HEADER_TYPE = "qaqh-wal-v1"
ROLE_ORDER = {"system": 0, "user": 1, "assistant": 2, "tool": 3, "developer": 4}

# ── 路径定位 ────────────────────────────────────────────────────────────


def default_data_root() -> Path:
    """镜像 Rust 侧 qaqh_types::platform::data_dir() 的解析顺序。"""
    env = os.environ.get("QAQH_DATA_DIR")
    if env:
        return Path(env)
    if os.name == "nt":
        return Path(os.environ.get("USERPROFILE", "~")).expanduser() / ".qaqh"
    xdg = os.environ.get("XDG_CONFIG_HOME")
    base = Path(xdg) if xdg else Path.home() / ".config"
    return base / "qaqh"


def resolve_sessions_dir(root: str | None) -> Path:
    """root 可以指向 data_root，也可以直接指向 sessions/ 目录。"""
    if root:
        p = Path(root).expanduser()
        if p.name == "sessions":
            return p
        return p / "sessions"
    data_root = default_data_root()
    # 尊重数据根标记（多实例/自定义根时以标记为准）。
    marker = data_root / ".qaqh-data-root.json"
    if marker.is_file():
        try:
            canonical = json.loads(marker.read_text(encoding="utf-8")).get("canonicalRoot")
            if canonical:
                return Path(canonical) / "sessions"
        except (OSError, ValueError):
            pass
    return data_root / "sessions"


def list_session_dirs(sessions_dir: Path) -> list[Path]:
    if not sessions_dir.is_dir():
        return []
    return sorted(
        (d for d in sessions_dir.iterdir() if d.is_dir() and (d / "messages.jsonl").exists()),
        key=lambda d: d.stat().st_mtime,
        reverse=True,
    )


def pick_session(sessions_dir: Path, seed: str | None) -> Path:
    if seed:
        d = sessions_dir / seed
        if not d.is_dir():
            die(f"session '{seed}' not found under {sessions_dir}")
        return d
    active = sessions_dir.parent / ".active_session"
    if active.is_file():
        name = active.read_text(encoding="utf-8").strip()
        if name and (sessions_dir / name).is_dir():
            return sessions_dir / name
    dirs = list_session_dirs(sessions_dir)
    if not dirs:
        die(f"no sessions with messages.jsonl under {sessions_dir}")
    return dirs[0]


def die(msg: str) -> None:
    print(f"session-forensics: {msg}", file=sys.stderr)
    raise SystemExit(2)


# ── 读取与解析 ──────────────────────────────────────────────────────────


def load_json(path: Path, default=None):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return default


def load_jsonl(path: Path) -> tuple[list, int]:
    """返回 (解析成功的对象列表, 撕裂/坏行数)。追加写文件可能有半行。"""
    out, torn = [], 0
    if not path.is_file():
        return out, torn
    with path.open(encoding="utf-8", errors="replace") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                out.append(json.loads(line))
            except ValueError:
                torn += 1
    return out, torn


class Session:
    def __init__(self, path: Path):
        self.dir = path
        self.seed = path.name
        self.meta = load_json(path / "meta.json", {}) or {}
        self.messages, self.torn = load_jsonl(path / "messages.jsonl")
        self.todo = load_json(path / "todo.json", {}) or {}
        self.outbox, _ = load_jsonl(path / "tool_outbox.wal")
        self.code_stats, _ = load_jsonl(path / "code_stats.jsonl")
        self.wal_ops, self.wal_header = self._load_wal(path / "messages.wal")

    def compaction_summaries(self) -> list[dict]:
        """messages.jsonl 里的 `[Compacted N turns]` 合成消息（route 1 压缩真相）。"""
        return [
            m for m in self.messages
            if any(
                b.get("type") == "text" and str(b.get("text", "")).startswith("[Compacted ")
                for b in blocks(m)
            )
        ]

    @staticmethod
    def _load_wal(path: Path):
        ops, header = [], None
        for obj in load_jsonl(path)[0]:
            if obj.get("type") == WAL_HEADER_TYPE:
                header = obj
            else:
                ops.append(obj)
        return ops, header

    # ── 查询辅助 ──
    def by_id(self, msg_id: int):
        for m in self.messages:
            if m.get("msg_id") == msg_id:
                return m
        return None

    def iter_role(self, role: str):
        for m in self.messages:
            if m.get("role") == role:
                yield m


def blocks(msg: dict) -> list[dict]:
    c = msg.get("content")
    return [b for b in c if isinstance(b, dict)] if isinstance(c, list) else []


def text_of(msg: dict) -> str:
    return "\n".join(b.get("text", "") for b in blocks(msg) if b.get("type") == "text")


def reasoning_of(msg: dict) -> str:
    return "\n".join(b.get("reasoning", "") for b in blocks(msg) if b.get("type") == "reasoning")


def tool_uses(msg: dict) -> list[dict]:
    return [b for b in blocks(msg) if b.get("type") == "tool_use"]


def tool_results(msg: dict) -> list[dict]:
    return [b for b in blocks(msg) if b.get("type") == "tool_result"]


def result_status(block: dict) -> str:
    r = block.get("result")
    return r.get("status", "?") if isinstance(r, dict) else "?"


def result_text(block: dict) -> str:
    r = block.get("result")
    if not isinstance(r, dict):
        return ""
    model = r.get("model") or {}
    if isinstance(model, dict) and model.get("text"):
        return model["text"]
    return r.get("summary", "") or ""


# 工具输入里值得展示的字段（按优先级）。
_INPUT_KEYS = (
    "argv", "command", "path", "file_path", "pattern", "old_string", "new_string",
    "requests", "hunks", "patch", "agent_name", "task_description", "prompt",
)


def summarize_input(name: str, inp) -> str:
    if not isinstance(inp, dict):
        return _clip(str(inp), 160)
    if name == "apply_patch" and isinstance(inp.get("patch"), str):
        return _clip(_patch_files(inp["patch"]), 200) or "apply_patch"
    if name in ("write", "edit", "read", "read_image") and "path" in inp:
        return str(inp["path"])
    if name == "edit_file" and "file_path" in inp:
        return str(inp["file_path"])
    if name == "read" and isinstance(inp.get("requests"), list):
        paths = [r.get("path") for r in inp["requests"] if isinstance(r, dict)]
        return ", ".join(str(p) for p in paths if p)
    if name == "exec":
        if inp.get("argv"):
            return _clip(" ".join(map(str, inp["argv"])), 200)
        if inp.get("command"):
            return _clip(str(inp["command"]), 200)
    if name == "grep":
        parts = [str(inp.get("pattern", ""))]
        if inp.get("paths"):
            parts.append("in " + ",".join(map(str, inp["paths"])))
        return " ".join(parts)
    for k in _INPUT_KEYS:
        if k in inp:
            return f"{k}={_clip(json.dumps(inp[k], ensure_ascii=False), 160)}"
    keys = ", ".join(list(inp)[:6])
    return f"{{{keys}}}" if keys else "{}"


_PATCH_FILE_RE = re.compile(r"^\*\*\*\s+(?:Add|Update|Delete)\s+File:\s*(.+?)\s*$", re.M)


def _patch_files(patch: str) -> str:
    files = _PATCH_FILE_RE.findall(patch or "")
    return ", ".join(dict.fromkeys(files))


# 从工具输入里收集文件路径（启发式，键名含 path/file）。
_PATHY_KEYS = ("path", "file_path", "file_paths", "files", "target_path", "source_path")


def collect_paths(name: str, inp) -> list[str]:
    found: list[str] = []
    if isinstance(inp, dict):
        for k, v in inp.items():
            if k in _PATHY_KEYS or k.endswith("_path"):
                if isinstance(v, str):
                    found.append(v)
                elif isinstance(v, list):
                    found += [x for x in v if isinstance(x, str)]
        if isinstance(inp.get("requests"), list):
            found += [r["path"] for r in inp["requests"] if isinstance(r, dict) and r.get("path")]
        if name == "apply_patch" and isinstance(inp.get("patch"), str):
            found += _PATCH_FILE_RE.findall(inp["patch"])
    return [p for p in dict.fromkeys(found) if p]


# ── 渲染 ────────────────────────────────────────────────────────────────


def _clip(s: str, n: int) -> str:
    s = s or ""
    if len(s) <= n:
        return s
    return s[:n] + f" …[+{len(s) - n} chars]"


def fmt_ts(epoch) -> str:
    try:
        dt = datetime.fromtimestamp(int(epoch))
        return dt.strftime("%Y-%m-%d %H:%M:%S")
    except (TypeError, ValueError, OSError, OverflowError):
        return str(epoch)


def hr(title: str = "") -> str:
    return f"\n{'─' * 4} {title} " if title else "─" * 60


# ── 子命令实现 ──────────────────────────────────────────────────────────


def cmd_sessions(args) -> int:
    sessions_dir = resolve_sessions_dir(args.root)
    dirs = list_session_dirs(sessions_dir)
    rows = []
    for d in dirs:
        meta = load_json(d / "meta.json", {}) or {}
        rows.append({
            "seed": d.name,
            "updated": fmt_ts(meta.get("updated_at") or d.stat().st_mtime),
            "model": meta.get("model"),
            "effort": meta.get("effort"),
            "messages": meta.get("message_count"),
            "turns": meta.get("turn_count"),
            "cwd": meta.get("cwd"),
            "title": meta.get("title"),
        })
    if args.json:
        print(json.dumps({"sessions_dir": str(sessions_dir), "sessions": rows},
                         ensure_ascii=False, indent=2))
        return 0
    print(f"sessions dir: {sessions_dir}")
    print(f"{'seed':<12}{'updated':<21}{'model':<26}{'msgs':>6}  cwd")
    for r in rows:
        print(f"{r['seed']:<12}{r['updated']:<21}{str(r['model']):<26}{str(r['messages']):>6}  {r['cwd']}")
    return 0


def cmd_info(args) -> int:
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    m = s.meta
    summaries = s.compaction_summaries()
    latest_summary = summaries[-1] if summaries else None
    info = {
        "seed": s.seed,
        "dir": str(s.dir),
        "model": m.get("model"),
        "effort": m.get("effort"),
        "cwd": m.get("cwd"),
        "created_at": fmt_ts(m.get("created_at")),
        "updated_at": fmt_ts(m.get("updated_at")),
        "message_count_meta": m.get("message_count"),
        "messages_on_disk": len(s.messages),
        "torn_lines": s.torn,
        "turn_count": m.get("turn_count"),
        # route 1 压缩事实：水位在 meta.json，摘要在 messages.jsonl 里。
        "compact_skip": m.get("compact_skip"),
        "compact_covered_through_msg_id": m.get("compact_covered_through_msg_id"),
        "compaction_summaries": len(summaries),
        "latest_summary_msg_id": latest_summary.get("msg_id") if latest_summary else None,
        "wal_header": s.wal_header,
        "wal_ops": len(s.wal_ops),
        "todo_items": len(s.todo.get("items", []) or []),
        "usage_totals": m.get("usage_totals"),
        "last_usage": m.get("last_usage"),
    }
    if args.json:
        print(json.dumps(info, ensure_ascii=False, indent=2))
        return 0
    print(f"session {s.seed}  ({s.dir})")
    for k in ("model", "effort", "cwd", "created_at", "updated_at",
              "message_count_meta", "messages_on_disk", "torn_lines", "turn_count",
              "compact_skip", "compact_covered_through_msg_id", "compaction_summaries",
              "latest_summary_msg_id", "wal_ops", "todo_items"):
        print(f"  {k:<30} {info[k]}")
    if info["usage_totals"]:
        u = info["usage_totals"]
        print(f"  usage_totals           total={u.get('total_tokens')} "
              f"prompt={u.get('prompt_tokens')} cache_hit={u.get('prompt_cache_hit_tokens')}")
    if info["wal_ops"]:
        print("  ⚠ messages.wal 有未 drain 的 op —— 内存态可能领先盘面")
    return 0


def cmd_users(args) -> int:
    """用户原话是压缩后最容易失真的部分——逐条列出，带 msg_id 与长度。"""
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    users = list(s.iter_role("user"))
    if args.json:
        print(json.dumps([{"msg_id": m.get("msg_id"), "name": m.get("name"),
                           "text": text_of(m)} for m in users], ensure_ascii=False, indent=2))
        return 0
    print(f"session {s.seed}: {len(users)} user message(s)")
    for m in users:
        txt = text_of(m)
        name = f" name={m['name']}" if m.get("name") else ""
        print(f"\n[msg {m.get('msg_id')}]{name} ({len(txt)} chars)")
        print(_clip(txt, args.max_chars))
    return 0


def cmd_search(args) -> int:
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    try:
        rx = re.compile(args.pattern, 0 if args.case_sensitive else re.IGNORECASE)
    except re.error as e:
        die(f"bad regex: {e}")
    hits = 0
    for m in s.messages:
        if args.role and m.get("role") != args.role:
            continue
        mid = m.get("msg_id")
        if args.since is not None and (mid or 0) < args.since:
            continue
        if args.until is not None and (mid or 0) > args.until:
            continue
        for kind, txt in (("text", text_of(m)), ("reasoning", reasoning_of(m))):
            for mt in rx.finditer(txt):
                hits += 1
                start = max(0, mt.start() - args.context)
                end = min(len(txt), mt.end() + args.context)
                snippet = txt[start:end].replace("\n", " ⏎ ")
                print(f"[msg {mid}] {m.get('role')}/{kind}: …{snippet}…")
                if hits >= args.max:
                    print(f"… hit limit {args.max} reached (use --max to raise)")
                    return 0
    if hits == 0:
        print("no matches")
    return 0


def cmd_tools(args) -> int:
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    calls = []
    for m in s.messages:
        for b in tool_uses(m):
            calls.append((m.get("msg_id"), b.get("name"), b.get("input"), b.get("id")))
    if args.name:
        calls = [c for c in calls if c[1] == args.name]
    total = len(calls)
    if args.last:
        calls = calls[-args.last:]
    if args.json:
        print(json.dumps([{"msg_id": mid, "name": n, "summary": summarize_input(n, inp),
                           "call_id": cid} for mid, n, inp, cid in calls],
                         ensure_ascii=False, indent=2))
        return 0
    print(f"session {s.seed}: showing {len(calls)}/{total} tool call(s)")
    for mid, n, inp, _cid in calls:
        print(f"[msg {mid:>5}] {n:<14} {summarize_input(n, inp)}")
    return 0


def cmd_files(args) -> int:
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    counts: dict[str, int] = {}
    first_seen: dict[str, int] = {}
    for m in s.messages:
        for b in tool_uses(m):
            for p in collect_paths(b.get("name", ""), b.get("input")):
                counts[p] = counts.get(p, 0) + 1
                first_seen.setdefault(p, m.get("msg_id") or 0)
    stats = {}
    for row in s.code_stats:
        f = row.get("file")
        if f:
            stats.setdefault(f, [0, 0, 0])
            stats[f][0] += row.get("lines_added", 0) or 0
            stats[f][1] += row.get("lines_removed", 0) or 0
            stats[f][2] += 1
    if args.json:
        print(json.dumps({"paths": [{"path": p, "tool_refs": c, "first_msg": first_seen[p],
                                     "code_stats": stats.get(p)}
                                    for p, c in sorted(counts.items(), key=lambda kv: -kv[1])]},
                         ensure_ascii=False, indent=2))
        return 0
    print(f"session {s.seed}: {len(counts)} distinct path(s) referenced by tools")
    for p, c in sorted(counts.items(), key=lambda kv: -kv[1]):
        extra = ""
        if p in stats:
            a, r, n = stats[p]
            extra = f"  [code_stats: +{a}/-{r} over {n} edit(s)]"
        print(f"  {c:>3}×  msg{first_seen[p]:<6} {p}{extra}")
    if stats:
        print(hr("code_stats.jsonl files"))
        for f, (a, r, n) in stats.items():
            if f not in counts:
                print(f"  {n:>3}×  {f}  (+{a}/-{r})")
    return 0


def cmd_show(args) -> int:
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    m = s.by_id(args.msg_id)
    if m is None:
        die(f"msg_id {args.msg_id} not found (session {s.seed})")
    if args.json:
        print(json.dumps(m, ensure_ascii=False, indent=2))
        return 0
    print(f"[msg {m.get('msg_id')}] role={m.get('role')} name={m.get('name')}")
    for b in blocks(m):
        t = b.get("type")
        if t == "text":
            print(hr("text"))
            print(_clip(b.get("text", ""), args.max_chars))
        elif t == "reasoning":
            print(hr("reasoning"))
            print(_clip(b.get("reasoning", ""), args.max_chars))
        elif t == "tool_use":
            print(hr(f"tool_use {b.get('name')} id={b.get('id')}"))
            print(_clip(json.dumps(b.get("input"), ensure_ascii=False, indent=2), args.max_chars))
        elif t == "tool_result":
            print(hr(f"tool_result status={result_status(b)} for={b.get('tool_use_id')}"))
            print(_clip(result_text(b), args.max_chars))
        else:
            print(hr(t))
            print(_clip(json.dumps(b, ensure_ascii=False), args.max_chars))
    return 0


def cmd_todo(args) -> int:
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    t = s.todo or {}
    if args.json:
        print(json.dumps(t, ensure_ascii=False, indent=2))
        return 0
    items = t.get("items", []) or []
    print(f"session {s.seed}: {len(items)} todo item(s)  "
          f"mode={t.get('mode')} current={t.get('current_id')}")
    for it in items:
        print(f"  [{it.get('status'):<9}] {it.get('id')}: {it.get('title')}")
        if it.get("evidence") and it.get("status") == "completed":
            print(f"              ↳ {_clip(it['evidence'], 200)}")
    return 0


def cmd_wal(args) -> int:
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    if not s.wal_ops and not s.wal_header:
        print("messages.wal: empty or absent")
        return 0
    print(f"header: {s.wal_header}")
    for op in s.wal_ops:
        seq = op.get("seq")
        kind = ", ".join(op.get("op", {}).keys())
        inner = next(iter(op.get("op", {}).values()), {})
        msgs = inner.get("messages", []) if isinstance(inner, dict) else []
        ids = [m.get("msg_id") for m in msgs if isinstance(m, dict)]
        print(f"  seq {seq}: {kind}  msgs={ids}  model={inner.get('model') if isinstance(inner, dict) else ''}")
    print("⚠ 这些 op 表示内存态可能领先 messages.jsonl（未被 drain）")
    return 0


def cmd_timeline(args) -> int:
    """按 msg_id 折叠成紧凑时间线，用来在压缩后快速定位'某件事发生在哪。"""
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    for m in s.messages:
        mid = m.get("msg_id")
        role = m.get("role")
        parts = []
        for b in blocks(m):
            t = b.get("type")
            if t == "text":
                parts.append("text:" + _clip(b.get("text", "").replace("\n", " "), args.max_chars))
            elif t == "reasoning":
                parts.append(f"reasoning({len(b.get('reasoning', ''))}c)")
            elif t == "tool_use":
                parts.append(f"{b.get('name')}→{summarize_input(b.get('name',''), b.get('input'))}")
            elif t == "tool_result":
                parts.append(f"result[{result_status(b)}]:" + _clip(result_text(b).replace("\n", " "), 80))
        print(f"{mid:>5} {role:<9} " + " | ".join(parts))
    return 0


def cmd_evidence(args) -> int:
    """压缩后恢复用的「取证包」：一次拿到最该记住的事实。"""
    s = Session(pick_session(resolve_sessions_dir(args.root), args.session))
    m = s.meta
    print("=" * 72)
    print(f"SESSION EVIDENCE PACK — {s.seed}")
    print("=" * 72)
    print(f"model={m.get('model')} effort={m.get('effort')} cwd={m.get('cwd')}")
    print(f"created={fmt_ts(m.get('created_at'))} updated={fmt_ts(m.get('updated_at'))}")
    print(f"messages_on_disk={len(s.messages)} (meta says {m.get('message_count')}) "
          f"torn={s.torn}")
    summaries = s.compaction_summaries()
    if summaries:
        latest = summaries[-1]
        covered = m.get("compact_covered_through_msg_id")
        print(f"compaction: {len(summaries)} checkpoint message(s) in messages.jsonl; "
              f"latest at msg {latest.get('msg_id')}; watermark "
              f"compact_covered_through_msg_id={covered}")
    print("source of truth: messages.jsonl is append-only and NOT compacted — "
          "cite msg_ids from it.")

    # 1) 用户原话（压缩后最易失真）
    users = list(s.iter_role("user"))
    print(hr(f"USER MESSAGES ({len(users)}) — verbatim"))
    for msg in users:
        txt = text_of(msg)
        name = f" name={msg['name']}" if msg.get("name") else ""
        print(f"\n[msg {msg.get('msg_id')}]{name} ({len(txt)}c)")
        print(_clip(txt, args.max_chars))

    # 2) todo 现状
    items = s.todo.get("items", []) or []
    if items:
        print(hr(f"TODO ({len(items)}) current={s.todo.get('current_id')}"))
        for it in items:
            print(f"  [{it.get('status'):<9}] {it.get('id')}: {it.get('title')}")
            if it.get("evidence"):
                print(f"              ↳ {_clip(it['evidence'], 160)}")

    # 3) 最近的工具调用
    calls = [(mm.get("msg_id"), b.get("name"), b.get("input"))
             for mm in s.messages for b in tool_uses(mm)]
    if calls:
        print(hr(f"LAST {min(args.last, len(calls))} TOOL CALLS (of {len(calls)})"))
        for mid, n, inp in calls[-args.last:]:
            print(f"[msg {mid:>5}] {n:<14} {summarize_input(n, inp)}")

    # 4) 触碰过的文件
    counts: dict[str, int] = {}
    for mm in s.messages:
        for b in tool_uses(mm):
            for p in collect_paths(b.get("name", ""), b.get("input")):
                counts[p] = counts.get(p, 0) + 1
    if counts:
        print(hr(f"FILES TOUCHED ({len(counts)}) — top {min(20, len(counts))}"))
        for p, c in sorted(counts.items(), key=lambda kv: -kv[1])[:20]:
            print(f"  {c:>3}×  {p}")

    # 5) 健康告警
    warns = []
    if s.torn:
        warns.append(f"{s.torn} torn/unparseable line(s) in messages.jsonl")
    if s.wal_ops:
        warns.append(f"{len(s.wal_ops)} un-drained op(s) in messages.wal (memory ahead of disk)")
    if warns:
        print(hr("WARNINGS"))
        for w in warns:
            print("  ⚠ " + w)
    print()
    return 0


# ── selftest ────────────────────────────────────────────────────────────


def cmd_selftest(args) -> int:
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        sd = root / "sessions" / "abcd1234"
        sd.mkdir(parents=True)
        (root / ".active_session").write_text("abcd1234", encoding="utf-8")
        (sd / "meta.json").write_text(json.dumps({
            "seed": "abcd1234", "created_at": 1_700_000_000, "updated_at": 1_700_000_100,
            "model": "test-model", "effort": "high", "message_count": 4, "turn_count": 1,
            "cwd": "/tmp/ws", "usage_totals": {"total_tokens": 42},
        }), encoding="utf-8")
        msgs = [
            {"msg_id": 1, "role": "system", "content": [{"type": "text", "text": "sys"}]},
            {"msg_id": 2, "role": "user", "content": [{"type": "text", "text": "hello 11155 bug"}]},
            {"msg_id": 3, "role": "assistant", "content": [
                {"type": "reasoning", "reasoning": "think"},
                {"type": "text", "text": "ok"},
                {"type": "tool_use", "id": "c1", "name": "exec",
                 "input": {"argv": ["bash", "-lc", "ls"]}},
            ]},
            {"msg_id": 4, "role": "tool", "content": [{"type": "tool_result",
             "tool_use_id": "c1", "result": {"status": "ok", "summary": "done",
                                             "model": {"text": "file list"}}}]},
        ]
        with (sd / "messages.jsonl").open("w", encoding="utf-8") as fh:
            for x in msgs:
                fh.write(json.dumps(x, ensure_ascii=False) + "\n")
            fh.write("{torn half line\n")  # 模拟撕裂行
        (sd / "meta.json").write_text(json.dumps({
            "seed": "abcd1234", "created_at": 1_700_000_000, "updated_at": 1_700_000_100,
            "model": "test-model", "effort": "high", "message_count": 4, "turn_count": 1,
            "cwd": "/tmp/ws", "usage_totals": {"total_tokens": 42},
            "compact_skip": 2, "compact_covered_through_msg_id": 3,
        }), encoding="utf-8")
        (sd / "todo.json").write_text(json.dumps({"items": [
            {"id": "T1", "title": "do thing", "status": "completed", "evidence": "done"}]}),
            encoding="utf-8")
        (sd / "messages.wal").write_text(
            json.dumps({"type": WAL_HEADER_TYPE, "next_seq": 5}) + "\n"
            + json.dumps({"seq": 5, "op": {"Append": {"seed": "abcd1234", "messages": msgs[3:],
                                                      "model": "test-model"}}}) + "\n",
            encoding="utf-8")

        s = Session(sd)
        assert s.seed == "abcd1234"
        assert len(s.messages) == 4, s.messages
        assert s.torn == 1, s.torn
        assert s.by_id(2)["role"] == "user"
        assert text_of(s.by_id(2)) == "hello 11155 bug"
        assert [b["name"] for b in tool_uses(s.by_id(3))] == ["exec"]
        assert result_status(tool_results(s.by_id(4))[0]) == "ok"
        assert result_text(tool_results(s.by_id(4))[0]) == "file list"
        assert summarize_input("exec", {"argv": ["bash", "-lc", "ls"]}) == "bash -lc ls"
        assert collect_paths("write", {"path": "/a/b.txt"}) == ["/a/b.txt"]
        assert _patch_files("*** Begin Patch\n*** Update File: /x.rs\n") == "/x.rs"
        assert len(s.wal_ops) == 1 and s.wal_header["next_seq"] == 5
        assert pick_session(root / "sessions", None).name == "abcd1234"
        assert resolve_sessions_dir(str(root)).name == "sessions"
        # route 1 压缩事实：无 compact-context.json，摘要按消息前缀识别。
        assert not (sd / "compact-context.json").exists()
        assert s.meta.get("compact_covered_through_msg_id") == 3
        assert s.compaction_summaries() == []

    print("selftest: OK (parsing, blocks, tool summary, wal, session pick)")
    return 0


# ── CLI ─────────────────────────────────────────────────────────────────


def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="session_forensics.py",
        description="从 QAQ-Harness 会话原始日志里精准取证（只读）。",
    )
    p.add_argument("--root", help="data_root 或 sessions/ 目录（默认自动探测）")
    p.add_argument("--session", help="会话 seed（默认 .active_session 或最近更新）")
    p.add_argument("--json", action="store_true", help="输出 JSON")
    # 让 --root/--session/--json 在子命令之后也能写；SUPPRESS 保证
    # 子命令未提供时不会用 None 覆盖顶层已解析的值。
    def _common():
        # 每次新建：复用同一个 parent 实例会被 argparse 的 action.container
        # 劫持，导致前面注册的子命令丢掉这些选项。
        c = argparse.ArgumentParser(add_help=False)
        c.add_argument("--root", default=argparse.SUPPRESS)
        c.add_argument("--session", default=argparse.SUPPRESS)
        c.add_argument("--json", action="store_true", default=argparse.SUPPRESS)
        return c

    sub = p.add_subparsers(dest="cmd", required=True)
    _add_parser = getattr(sub, "add_parser")

    def add(name, **kw):
        return _add_parser(name, parents=[_common()], **kw)

    add("sessions", help="列出所有会话").set_defaults(func=cmd_sessions)
    add("info", help="会话元信息与文件健康").set_defaults(func=cmd_info)

    pu = add("users", help="逐条列出用户原话（带 msg_id）")
    pu.add_argument("--max-chars", type=int, default=2000)
    pu.set_defaults(func=cmd_users)

    ps = add("search", help="在消息里按正则检索")
    ps.add_argument("pattern")
    ps.add_argument("--role", choices=sorted(ROLE_ORDER))
    ps.add_argument("--since", type=int, help="msg_id 下界（含）")
    ps.add_argument("--until", type=int, help="msg_id 上界（含）")
    ps.add_argument("--context", type=int, default=60, help="匹配两侧上下文字符数")
    ps.add_argument("--max", type=int, default=50)
    ps.add_argument("--case-sensitive", action="store_true")
    ps.set_defaults(func=cmd_search)

    pt = add("tools", help="列出工具调用")
    pt.add_argument("--name", help="只看法定工具名")
    pt.add_argument("--last", type=int, default=0, help="只看最后 N 条")
    pt.set_defaults(func=cmd_tools)

    add("files", help="工具触碰过的文件路径 + code_stats").set_defaults(func=cmd_files)

    ph = add("show", help="完整打印某条消息（按 msg_id）")
    ph.add_argument("msg_id", type=int)
    ph.add_argument("--max-chars", type=int, default=8000)
    ph.set_defaults(func=cmd_show)

    add("todo", help="打印 todo.json").set_defaults(func=cmd_todo)
    add("wal", help="打印 messages.wal 未 drain 的 op").set_defaults(func=cmd_wal)

    ptl = add("timeline", help="紧凑时间线")
    ptl.add_argument("--max-chars", type=int, default=120)
    ptl.set_defaults(func=cmd_timeline)

    pe = add("evidence", help="压缩后恢复用取证包")
    pe.add_argument("--last", type=int, default=15, help="取证包里保留多少条工具调用")
    pe.add_argument("--max-chars", type=int, default=1500)
    pe.set_defaults(func=cmd_evidence)

    add("selftest", help="内置自检（不触碰真实会话）").set_defaults(func=cmd_selftest)
    return p


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    raise SystemExit(main())
