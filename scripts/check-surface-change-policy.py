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
# 这些分支已经是"冻结线"，refreeze 必须落到新线（v3.1…）。
FROZEN_BRANCHES = ("LTS-v3.0",)


def git(*args: str) -> str:
    return subprocess.run(
        ["git", *args], cwd=ROOT, text=True, capture_output=True, check=True
    ).stdout


def is_placeholder(sha: str) -> bool:
    # 新建分支的 before sha 全为 0；此时没有可比基线。
    return not sha or set(sha) == {"0"}


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
    required = REFREEZE if artifacts else TIGHTEN

    evidence = git("log", f"{args.base}..{args.head}", "--format=%s%n%b") + "\n" + os.environ.get(
        "SURFACE_LABELS", ""
    )
    declared = [marker for marker in (REFREEZE, TIGHTEN) if marker in evidence]

    print(f"surface-policy: 金标准改动 {len(golden)} 个文件"
          f"（导出物 {len(artifacts)} / 快照 {len(snapshots)}）⇒ 需要标记 `[{required}]`")
    for path in golden[:12]:
        print(f"    {path}")
    if len(golden) > 12:
        print(f"    …（另 {len(golden) - 12} 个）")

    problems: list[str] = []
    if required not in evidence:
        problems.append(
            f"缺少必需标记 `[{required}]`（写进 commit subject/body 或 PR label）"
        )
        if declared and required not in declared:
            problems.append(
                f"声明了 `[{declared[0]}]` 但改动落点要求 `[{required}]`："
                + ("只动了 snapshots/*，属收紧而非换锁" if required == TIGHTEN
                   else "动了导出物，contract_hash 会变，属换锁")
            )
    if artifacts and required == REFREEZE and args.target in FROZEN_BRANCHES:
        problems.append(
            f"`{args.target}` 是已冻结线：contract-refreeze 只能进新 LTS 线"
            "（如 LTS-v3.1），并按手册 §1 重切 tag + bump contract_revision。"
        )

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

    print(f"surface-policy: OK — 按 `[{required}]` 处理"
          + ("" if not artifacts else "（换锁：禁入已冻结线）"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
