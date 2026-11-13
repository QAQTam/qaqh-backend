# 分工（v3.0 收口合并裁定）：本脚本盯「源码冻结面 → §7 登记」（LTS 线，lts-api-guard.yml
# 的 change-policy 作业）；check-surface-change-policy.py 盯「金标准更新 → 三分类」
# （surface-policy.yml，main/LTS 共用）。两者互补不重叠，勿合并成一个接口。
#!/usr/bin/env python3
"""锁内偏离必须留痕 —— 把 §1.5 从人工纪律迁成机器断言（B 方案）。

判据只有一条：一次改动触碰了「前端可见的冻结面」文件，就必须在**同一次改动**里
往 `docs/lts-v3.1-maintenance.md` §7 加一条登记（标题行 `### 7.<n> …`）。
当前冻结线 = LTS-v3.1（2026-11-13 切线，ADR-0010）；v3.0 账本留作历史。

为什么盯文件而不是盯 `contract_hash`：`contract_hash` 由导出物算出，连一句
doc 注释都能让它漂移（见 §7.2 追认的那次）；把断言建在影子上是把 CI 变成导出
重跑器，而文件集合是它的成因。

为什么这条断言自己需要自检：`freeze` 比的 golden 是被改对象之一，守卫脚本也是
（ADR-0007 §问题 1）。`--self-test` 把「判据本身」钉成 CI 可执行的正负样本 ——
谁把 `FROZEN` 删短、把 `ENTRY` 放宽，这一步就先红。
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys

# 前端可见面：动这些就是动用户能看到的形状（§1.1 路由 / §1.3 方法表 / §1.4 命令与事件面）。
FROZEN_PREFIXES = (
    "docs/api-surface/",                    # 契约导出物（含金标准）
    "crates/qaqh-ringing-contract/",        # 会话事件/状态契约类型
    "crates/qaqh-ringing/src/v2/",          # 会话面 wire 类型与 revision 常量
    "crates/qaqh-ringing/src/command.rs",   # 命令面
    "crates/qaqh-client/",                  # 公开 facade
    "tools/qaqh-contract-export/",          # 导出器 = 锁的度量仪本身
)

# 唯一允许写登记的文件与条目形状。当前冻结线的账本（切线时随指向一起改）。
LEDGER_FILE = "docs/lts-v3.1-maintenance.md"
LEDGER_ENTRY = re.compile(r"^\+### 7\.\d+ \S")


def git(*args: str) -> str:
    result = subprocess.run(
        ["git", *args], check=True, text=True, capture_output=True
    )
    return result.stdout


def frozen_only(paths: list[str]) -> list[str]:
    """按判据筛出前端可见冻结面。单独成函数,是为了让自检能测到"筛选"这一步本身。"""
    return [path for path in paths if path.startswith(FROZEN_PREFIXES)]


def verdict(touched_frozen: list[str], ledger_entries_added: list[str]) -> list[str]:
    """返回违规说明；空列表 = 通过。与 git 无关，便于自检。"""
    if not touched_frozen:
        return []
    if ledger_entries_added:
        return []
    head = "\n".join(f"  - {path}" for path in touched_frozen)
    return [
        "触碰前端可见冻结面却没有 §7 登记（§1.5 的机器化）：\n"
        f"{head}\n"
        f"请在 {LEDGER_FILE} 的 §7 下新增一条 `### 7.<n> <一句话>`，写清变更、"
        "兼容性判定（additive / 破坏性）、是否 bump contract_revision、以及"
        "为何机器闸门放行。破坏性偏离不属于本锁，必须开新 LTS 线（v3.2）。"
    ]


class NewBranchPush(Exception):
    """新建分支的首次 push：没有 before 端，本次跳过比对（如实报出，不算通过也不算失败）。"""


def rev_exists(rev: str) -> bool:
    result = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", rev],
        capture_output=True,
        text=True,
    )
    return result.returncode == 0


def resolve_range(args: argparse.Namespace) -> tuple[str, str]:
    """确定比较区间。PR：merge-base(base, head)..head；push：before..after。

    解析不出来就抛异常（调用方转成退出码 2），绝不返回一个"看起来能跑"的假区间——
    这条断言的意义是拦人，静默降级等于把闸门拆了还报告成功。
    """
    if args.base or args.head:
        if not (args.base and args.head):
            raise ValueError("--base 与 --head 必须成对给出")
        return merge_base(args.base, args.head), args.head

    event = os.environ.get("GITHUB_EVENT_NAME", "")
    sha = os.environ.get("GITHUB_SHA", "").strip()
    if event == "pull_request":
        base_ref = os.environ.get("GITHUB_BASE_REF", "").strip()
        candidates = [f"origin/{base_ref}", base_ref] if base_ref else []
        base = next((rev for rev in candidates if rev and rev_exists(rev)), "")
        if not base or not sha or not rev_exists(sha):
            raise ValueError(
                f"pull_request 事件下解析不出比较区间（base={base_ref!r} → {base!r}, "
                f"head={sha!r}）；请显式传 --base/--head"
            )
        return merge_base(base, sha), sha
    if event == "push":
        before = os.environ.get("GITHUB_EVENT_BEFORE", "").strip()
        if not before or set(before) == {"0"}:
            raise NewBranchPush("新建分支的首次 push：没有 before 端")
        if not sha:
            raise ValueError("push 事件缺少 GITHUB_SHA")
        return before, sha
    raise ValueError(f"不是可识别的事件（GITHUB_EVENT_NAME={event!r}），请显式传 --base/--head")


def merge_base(base: str, head: str) -> str:
    # 不回退成 base 本身：那会把"两个端点不同源"伪装成"差异很大"，判据就失真了。
    return git("merge-base", base, head).strip()


def collect(base: str, head: str) -> tuple[list[str], list[str]]:
    changed = [line for line in git("diff", "--name-only", f"{base}..{head}").splitlines() if line]
    frozen = frozen_only(changed)
    diff = git("diff", "--unified=0", f"{base}..{head}", "--", LEDGER_FILE)
    entries = [line for line in diff.splitlines() if ledger_line_added(line)]
    return frozen, entries


def ledger_line_added(line: str) -> bool:
    """这一行是不是「新增了一条 §7 登记」。抽成函数,让判据可被直接喂样本。"""
    return bool(LEDGER_ENTRY.match(line))


def self_test() -> int:
    failed = 0

    def check(label: str, actual: object, expected: object) -> None:
        nonlocal failed
        if actual != expected:
            failed += 1
            print(f"self-test FAIL: {label}: got {actual!r} expect {expected!r}")

    # 1) 判据覆盖面：少一个前缀,那类漂移就能悄悄过去,所以逐个钉住。
    for probe in FROZEN_PREFIXES:
        check(f"冻结面前缀 {probe}", frozen_only([f"{probe}x.rs"]), [f"{probe}x.rs"])
    # 非冻结面必须被筛掉（否则实现级改动也会被要求登记,判据就变成噪音）。
    for probe in [
        "crates/qaqh-file-tools/src/code_delta.rs",
        "crates/qaqh-agent-runtime/src/provider_job.rs",
        "docs/lts-v3.1-maintenance.md",
    ]:
        check(f"非冻结面 {probe}", frozen_only([probe]), [])

    # 2) 留痕判据：冻结面 + 无登记 = 红；冻结面 + 有登记 = 绿；无冻结面 = 不需要登记。
    def passes(frozen: list[str], entries: list[str]) -> bool:
        return not verdict(frozen, entries)

    check("冻结面无登记应红", passes(["docs/api-surface/v3.0/manifest.json"], []), False)
    check("冻结面有登记应绿", passes(["docs/api-surface/v3.0/manifest.json"], ["### 7.3 x"]), True)
    check("无冻结面无需登记", passes([], []), True)

    # 3) 标题行正则：放宽它等于把闸门拆了,所以正反样本都钉。
    for sample, expected in [
        ("+### 7.3 锁的执法方式变更", True),
        ("+### 7.10 x", True),
        ("+随便一行说明", False),
        ("+### 8.1 不是 §7", False),
        ("+### 7.x 编号不是数字", False),
        (" ### 7.3 未新增(上下文行)", False),
    ]:
        check(f"登记标题判据 {sample!r}", ledger_line_added(sample), expected)

    total = len(FROZEN_PREFIXES) + 3 + 3 + 6
    print(f"self-test: {total} cases, {failed} failed")
    return 1 if failed else 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__ or "")
    parser.add_argument("--base", help="比较基线 ref（默认取 GitHub 事件环境）")
    parser.add_argument("--head", help="比较终点 ref（默认 GITHUB_SHA）")
    parser.add_argument("--self-test", action="store_true", help="跑判据自身的正负样本")
    args = parser.parse_args()

    if args.self_test:
        return self_test()

    try:
        base, head = resolve_range(args)
    except NewBranchPush as skip:
        print(f"change-policy: 跳过比对 —— {skip}", file=sys.stderr)
        return 0
    except (ValueError, subprocess.CalledProcessError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    if not rev_exists(base) or not rev_exists(head):
        print(f"error: 比较端点不存在：base={base!r} head={head!r}", file=sys.stderr)
        return 2
    frozen, entries = collect(base, head)
    problems = verdict(frozen, entries)
    if problems:
        for problem in problems:
            print(f"error: {problem}", file=sys.stderr)
        return 1
    scope = "无冻结面改动" if not frozen else f"{len(frozen)} 个冻结面文件 + §7 登记 {len(entries)} 条"
    print(f"change-policy ok ({base[:8]}..{head[:8]}): {scope}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
