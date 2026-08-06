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
  serverEpoch: string,
): RingingEventBatch {
  // M4：信封已移除 schema/version/channel/server_epoch（帧 id 携带 epoch/channel，
  // URL 携带协议版本）；事件自身 channel 仍校验与连接一致。
  if (
    !envelope.seed
    || !envelope.event_id
    || (envelope.event as { channel?: unknown }).channel !== channel
    || !Number.isSafeInteger(envelope.stream_seq)
    || envelope.stream_seq < 0
    || !Number.isSafeInteger(envelope.channel_seq)
    || envelope.channel_seq < 0
    || !Number.isSafeInteger(envelope.session_seq)
    || envelope.session_seq < 0
    || (envelope.state_revision != null
      && (!Number.isSafeInteger(envelope.state_revision) || envelope.state_revision < 0))
  ) {
    throw new Error("invalid Ringing stream sequence");
  }
  return {
    schema: "deepx.Ringing",
    version: 1,
    channel,
    seed: envelope.seed,
    server_epoch: serverEpoch,
    from_stream_seq: envelope.stream_seq,
    to_stream_seq: envelope.stream_seq,
    envelopes: [envelope],
  };
}

/** 校验 SSE id 的 cursor 形状；无效 id 不得推进流 cursor。 */
export function cursorFromSseId(id: string, channel: RingingChannel): number | null {
  const parts = id.split(":");
  if (parts.length !== 3 || parts[1] !== channel || !parts[0]) return null;
  const seq = Number(parts[2]);
  return Number.isSafeInteger(seq) && seq >= 0 ? seq : null;
}

/** 解析 `ringing.reset_required` 的 data payload。 */
export function parseResetRequired(data: string): RingingResetRequired {
  return JSON.parse(data) as RingingResetRequired;
}

/** 从 `id: epoch:channel:seq` 提取 seq；格式不符返回 0。 */
export function cursorSeqFromId(id: string): number {
  const seq = Number(id.split(":").at(-1));
  return Number.isSafeInteger(seq) && seq >= 0 ? seq : 0;
}
