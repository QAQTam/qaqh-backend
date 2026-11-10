#!/usr/bin/env python3
"""Surface change policy — ADR-0007 决策 2/3 的机器化。

改 `docs/api-surface/**` 的提交必须自证它属于哪一类，两类不可混用：

- `contract-refreeze`：动了**导出物**（`manifest.json`、`types/`、`cs/`、`fixtures/`、
  `coverage.json` …）。即 `contract_hash` 会变 ⇒ 用户可见面变了 ⇒ 只允许进新 LTS 线，
  **禁止合入 `LTS-v3.0`**，且必须附 ADR（手册 §1"换锁不是冻结"）。
- `guard-tightening`：**只**动 `snapshots/*`（route/facade 基线）而导出物逐字节不变。
  即冻得更紧、不是改约 ⇒ 允许进 LTS，走补丁发版。

为什么必须分两类：否则"改进守卫自己"（例如 ADR-0003 把控制面方法钉进 routes.txt）
会被同一条"改金标准就得 refreeze"的规则一并堵死，逼人绕过规则而不是走通道。

识别方式：标记写在 commit subject/body（`[guard-tightening]` 之类）或 PR label
（workflow 通过 `SURFACE_LABELS` 注入）。

`--base` / `--head` / `--target` 缺省时读环境变量（CI 传入）。
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
GOLDEN_PREFIX = "docs/api-surface/"
SNAPSHOT_MARKER = "snapshots/"
REFREEZE = "contract-refreeze"
TIGHTEN = "guard-tightening"
DOCONLY = "doc-only-refresh"
# 这些分支已经是"冻结线"，refreeze 必须落到新线（v3.1…）。
FROZEN_BRANCHES = ("LTS-v3.0",)
# 只有字节记账作用的文件：它必然随任何产物变化而变化，不参与语义判断。
BOOKKEEPING = ("manifest.json",)
CODE_SUFFIXES = (".ts", ".cs")


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, text=True, capture_output=True, check=True
    ).stdout


def is_placeholder(sha: str) -> bool:
    # 新建分支的 before sha 全为 0；此时没有可比基线。
    return not sha or set(sha) == {"0"}


def show(rev: str, path: str) -> str | None:
    proc = subprocess.run(["git", "show", f"{rev}:{path}"], cwd=ROOT,
                          capture_output=True, text=True)
    return proc.stdout if proc.returncode == 0 else None


def wire_view(path: str, text: str) -> str:
    """产物里"客户端能观察到的部分"：去掉注释后的内容。

    注释不进 wire 语义 —— 但它进 `contract_hash`（哈希的是字节）。不区分这两件事的后果
    是真实发生过的：`ac54e508` 只给 `ModelCapabilities` 的 Rust doc 加了一段分层说明，
    `4c3a26be` 重切金标准后 `contract_hash` 由 `ead943ac…` 变成 `c25b08f1…`，
    于是一次**零 wire 影响**的文档改动被当成"换锁"。这里把它拆出来单独分类。
    只在代码后缀上去行尾注释，避免误伤 JSON 字符串里的 `//`。
    """
    if not path.endswith(CODE_SUFFIXES):
        return text
    lines: list[str] = []
    in_block = False
    for raw in text.splitlines():
        line = raw.strip()
        if in_block:
            if "*/" in line:
                in_block = False
            continue
        if line.startswith("/*"):
            if "*/" not in line:
                in_block = True
            continue
        if line.startswith("//"):
            continue
        lines.append(line.split(" // ")[0])
    return "\n".join(lines)


def artifacts_wire_changed(base: str, head: str, artifacts: list[str]) -> list[str]:
    """返回真正发生 wire 变化的产物；空列表 ⇒ 只有注释/记账在动。

    先用一次 `--name-status` 拿增删改状态（增/删必属 wire 变化），只对**修改过**的
    产物取两侧内容 —— 否则一份金标准 diff 会 spawn 上千次 git show。
    """
    listing = git("diff", "--name-status", f"{base}..{head}", "--", GOLDEN_PREFIX)
    status: dict[str, str] = {}
    for line in listing.splitlines():
        parts = line.split("\t")
        if len(parts) >= 2:
            status[parts[-1].strip()] = parts[0].strip()

    semantic: list[str] = []
    for path in artifacts:
        if path.rsplit("/", 1)[-1] in BOOKKEEPING:
            continue
        kind = status.get(path, "M")
        if kind.startswith("A"):
            semantic.append(f"artifact added: {path}")
            continue
        if kind.startswith("D"):
            semantic.append(f"artifact deleted: {path}")
            continue
        before, after = show(base, path), show(head, path)
        if before is None or after is None:
            semantic.append(f"artifact unavailable on one side ({kind}): {path}")
        elif wire_view(path, before) != wire_view(path, after):
            semantic.append(f"wire-differs: {path}")
    return semantic


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default=os.environ.get("SURFACE_BASE_SHA", ""))
    parser.add_argument("--head", default=os.environ.get("SURFACE_HEAD_SHA", "HEAD"))
    parser.add_argument("--target", default=os.environ.get("SURFACE_TARGET_BRANCH", ""))
    args = parser.parse_args()

    if is_placeholder(args.base):
        print("surface-policy: 无可比基线（新分支或首推），跳过；"
              "注意这意味着这次推送未经改动分类检查。")
        return 0

    changed = [
        line.strip()
        for line in git("diff", "--name-only", f"{args.base}..{args.head}").splitlines()
        if line.strip()
    ]
    golden = [path for path in changed if path.startswith(GOLDEN_PREFIX)]
    if not golden:
        print("surface-policy: 未触碰冻结金标准，规则不适用。")
        return 0

    artifacts = [path for path in golden if SNAPSHOT_MARKER not in path]
    snapshots = [path for path in golden if SNAPSHOT_MARKER in path]
    # 三分类，而不是两分类 —— 见 wire_view() 的记录：注释不进可观察契约，
    # 但它进 contract_hash（哈希字节）。把两者混为一谈会让"加一句文档说明"变成"换锁"。
    semantic = artifacts_wire_changed(args.base, args.head, artifacts) if artifacts else []
    if not artifacts:
        required = TIGHTEN
    elif semantic:
        required = REFREEZE
    else:
        required = DOCONLY

    evidence = git("log", f"{args.base}..{args.head}", "--format=%s%n%b") + "\n" + os.environ.get(
        "SURFACE_LABELS", ""
    )
    declared = [marker for marker in (REFREEZE, DOCONLY, TIGHTEN) if marker in evidence]

    print(f"surface-policy: 金标准改动 {len(golden)} 个文件"
          f"（导出物 {len(artifacts)} / 快照 {len(snapshots)} / wire 实变 {len(semantic)}）"
          f"⇒ 需要标记 `[{required}]`")
    for line in semantic[:10]:
        print(f"    {line}")
    if len(semantic) > 10:
        print(f"    …（另 {len(semantic) - 10} 项 wire 变化）")
    if not semantic and artifacts:
        print(f"    （{len(artifacts)} 个导出物只有注释或 manifest 记账在动，例：{artifacts[0]}）")

    problems: list[str] = []
    if required not in evidence:
        problems.append(
            f"缺少必需标记 `[{required}]`（写进 commit subject/body 或 PR label）"
        )
        if declared and required not in declared:
            problems.append(
                f"声明了 `[{declared[0]}]` 但改动落点要求 `[{required}]`："
                + ("只动了 snapshots/*，属收紧而非换锁" if required == TIGHTEN
                   else ("有 wire 实变，属换锁" if required == REFREEZE
                         else "wire 无变化，只是注释/记账，不该按换锁算"))
            )
    if required == REFREEZE and args.target in FROZEN_BRANCHES:
        problems.append(
            f"`{args.target}` 是已冻结线：contract-refreeze 只能进新 LTS 线"
            "（如 LTS-v3.1），并按手册 §1 重切 tag + bump contract_revision。"
        )
    # DOCONLY 允许进冻结线，但仍必须显式声明：谎报会被上面那份 wire 实变列表当场戳穿，
    # 所以这条不靠自觉，靠证据。

    if problems:
        print("surface-policy: FAILED", file=sys.stderr)
        for problem in problems:
            print(f"  - {problem}", file=sys.stderr)
        print(
            "  判据见 docs/adr/0007-refreeze-governance.md §决策 2 与 "
            "docs/adr/0003-route-registry-fail-closed.md §落地清单。",
            file=sys.stderr,
        )
        return 1

    meaning = {
        REFREEZE: "（换锁：wire 实变，禁入已冻结线，须 bump revision 并重切 tag）",
        DOCONLY: "（仅注释/记账变化：不动 wire，允许进已冻结线，无需 bump revision）",
        TIGHTEN: "（只收紧快照基线：允许进已冻结线）",
    }[required]
    print(f"surface-policy: OK — 按 `[{required}]` 处理{meaning}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
