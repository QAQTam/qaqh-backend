#!/usr/bin/env bash
# Ringing TS bindings 合并与漂移检查（PLAN：自动生成 + CI 检查漂移）。
#
# 用法:
#   bash scripts/ringing-bindings.sh            # 生成/更新 bindings
#   bash scripts/ringing-bindings.sh --check    # 仅检查漂移（CI 用，有差异时退出码 1）
#
# 源: crates/deepx-domain/bindings + crates/deepx-ringing/bindings（ts-rs 自动导出）
# 目标: apps/desktop/src/lib/types/ringing/
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SRC_DIRS=("$ROOT/crates/deepx-domain/bindings" "$ROOT/crates/deepx-ringing/bindings")
DEST="$ROOT/apps/desktop/src/lib/types/ringing"
CHECK_MODE=0
[[ "${1:-}" == "--check" ]] && CHECK_MODE=1

if [[ ! -d "${SRC_DIRS[0]}" || ! -d "${SRC_DIRS[1]}" ]]; then
  echo "error: bindings not generated — run: cargo test -p deepx-domain -p deepx-ringing" >&2
  exit 1
fi

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

# 递归合并两个 crate 的 bindings（含 serde_json/ 等子目录；冲突时后复制者覆盖）
for dir in "${SRC_DIRS[@]}"; do
  cp -rf "$dir"/. "$tmpdir"/
done

# 生成聚合入口 index.ts（各类型从 ts-rs 各自的文件中导出）
{
  echo "// 由 scripts/ringing-bindings.sh 自动生成，禁止手改。"
  echo "// 源：crates/deepx-domain + crates/deepx-ringing 的 ts-rs 导出。"
  echo "export type { RingingChannel } from \"./RingingChannel\";"
  echo "export type { Delivery } from \"./Delivery\";"
  echo "export type { DomainCommand } from \"./DomainCommand\";"
  echo "export type { ControlCommand } from \"./ControlCommand\";"
  echo "export type { ConversationCommand } from \"./ConversationCommand\";"
  echo "export type { ToolCommand } from \"./ToolCommand\";"
  echo "export type { ConversationMode } from \"./ConversationMode\";"
  echo "export type { ImageBlock } from \"./ImageBlock\";"
  echo "export type { AskAnswer } from \"./AskAnswer\";"
  echo "export type { DomainEvent } from \"./DomainEvent\";"
  echo "export type { ControlEvent } from \"./ControlEvent\";"
  echo "export type { ConversationEvent } from \"./ConversationEvent\";"
  echo "export type { ToolEvent } from \"./ToolEvent\";"
  echo "export type { RoundDeltaKind } from \"./RoundDeltaKind\";"
  echo "export type { ProviderToolState } from \"./ProviderToolState\";"
  echo "export type { CompactStatus } from \"./CompactStatus\";"
  echo "export type { DomainError } from \"./DomainError\";"
  echo "export type { ContentRef } from \"./ContentRef\";"
  echo "export type { ToolResult } from \"./ToolResult\";"
  echo "export type { AskMode } from \"./AskMode\";"
  echo "export type { AskQuestion } from \"./AskQuestion\";"
  echo "export type { AskResolution } from \"./AskResolution\";"
  echo "export type { PermissionCategory } from \"./PermissionCategory\";"
  echo "export type { PermissionRisk } from \"./PermissionRisk\";"
  echo "export type { SessionState } from \"./SessionState\";"
  echo "export type { ActivityState } from \"./ActivityState\";"
  echo "export type { AgentLifecycleState } from \"./AgentLifecycleState\";"
  echo "export type { ErrorScope } from \"./ErrorScope\";"
  echo "export type { TodoItem } from \"./TodoItem\";"
  echo "export type { SkillInfo } from \"./SkillInfo\";"
  echo "export type { NoticeLevel } from \"./NoticeLevel\";"
  echo "export type { RingingEvent } from \"./RingingEvent\";"
  echo "export type { RingingEventEnvelope } from \"./RingingEventEnvelope\";"
  echo "export type { RingingCommand } from \"./RingingCommand\";"
  echo "export type { RingingCommandEnvelope } from \"./RingingCommandEnvelope\";"
  echo "export type { RingingCommandAck } from \"./RingingCommandAck\";"
  echo "export type { RingingCommandAckStatus } from \"./RingingCommandAckStatus\";"
  echo "export type { RingingEventBatch } from \"./RingingEventBatch\";"
  echo "export type { RingingChannelSnapshot } from \"./RingingChannelSnapshot\";"
  echo "export type { ClientOpenRequest } from \"./ClientOpenRequest\";"
  echo "export type { ClientOpenResponse } from \"./ClientOpenResponse\";"
  echo "export type { CapabilityName } from \"./CapabilityName\";"
  echo "export type { RingingWorkerCommandEnvelope } from \"./RingingWorkerCommandEnvelope\";"
  echo "export type { RingingWorkerEventEnvelope } from \"./RingingWorkerEventEnvelope\";"
  echo "export type { WorkerDirection } from \"./WorkerDirection\";"
} > "$tmpdir/index.ts"

if [[ "$CHECK_MODE" -eq 1 ]]; then
  if ! diff -rq "$tmpdir" "$DEST" >/dev/null 2>&1; then
    echo "error: Ringing bindings drifted — run: bash scripts/ringing-bindings.sh" >&2
    exit 1
  fi
  echo "Ringing bindings up to date."
  exit 0
fi

rm -rf "$DEST"
cp -rf "$tmpdir" "$DEST"
echo "Ringing bindings synced to $DEST ($(find "$DEST" -name '*.ts' | wc -l) files)."
