// Ringing SSE 帧解析与信封 → batch 转换（Electron main 与浏览器调试桥共用）。
//
// 服务端帧格式（PLAN）：
// ```text
// id: <server_epoch>:<channel>:<stream_seq>
// event: <Ringing event type>        （如 tool_started；keepalive 只有注释行）
// data: <RingingEventEnvelope JSON>
// ```
// 特殊事件 `ringing.reset_required`：cursor 超出保留窗口，客户端必须经 HTTP
// 读取权威 snapshot 后继续。

import type {
  RingingChannel,
  RingingEventBatch,
  RingingEventEnvelope,
  RingingResetRequired,
} from "../lib/types/ringing";

export interface ParsedSseFrame {
  id: string;
  eventType: string;
  data: string;
}

/** 解析一个 SSE 帧（以空行分隔）。注释行（`: keepalive`）被忽略。 */
export function parseSseFrame(frame: string): ParsedSseFrame {
  let id = "";
  let eventType = "";
  let data = "";
  for (const line of frame.split("\n")) {
    if (line.startsWith(":")) continue;
    if (line.startsWith("id:")) id = line.slice(3).trim();
    else if (line.startsWith("event:")) eventType = line.slice(6).trim();
    else if (line.startsWith("data:")) data += line.slice(5).trim();
  }
  return { id, eventType, data };
}

/** 单信封 → 整 batch（main→renderer 一次 IPC，禁止逐事件展开）。 */
export function envelopeToBatch(
  channel: RingingChannel,
  envelope: RingingEventEnvelope,
): RingingEventBatch {
  return {
    channel,
    seed: envelope.seed,
    from_stream_seq: envelope.stream_seq,
    to_stream_seq: envelope.stream_seq,
    state_revision: envelope.state_revision ?? null,
    events: [envelope.event],
  };
}

/** 解析 `ringing.reset_required` 的 data payload。 */
export function parseResetRequired(data: string): RingingResetRequired {
  return JSON.parse(data) as RingingResetRequired;
}

/** 从 `id: epoch:channel:seq` 提取 seq；格式不符返回 0。 */
export function cursorSeqFromId(id: string): number {
  const parts = id.split(":");
  const seq = Number(parts[parts.length - 1]);
  return Number.isFinite(seq) ? seq : 0;
}
