// Ringing 命令/查询 renderer 入口。
//
// Ringing v1 命令入口。backend 模式在连接级 open 协商时固定；协商失败直接
// 暴露给 UI，绝不退回 legacy backend。
//
// 边界：
// - 带 files 的 send_message 经过 backend:request，由 Electron main 读取并上传
//   为 ContentRef；本地路径不会进入 Ringing 命令。
// - ack 只表示 accepted；业务结果经 Ringing 事件流（causation_id = command_id）
//   返回，本 helper 不等待终态。
// - Ringing V1 连接故障只向 UI 报错；不会建立 legacy IPC。

import type { RingingChannel } from "../lib/types/ringing";
import { request } from "./backendClient";

export function ringingCommandsEnabled(): boolean {
  return true;
}

interface RingingCommandSpec {
  channel: RingingChannel;
  command: unknown;
}

/** legacy 方法 → Ringing 频道命令映射；附件场景交给 Electron main 补齐 ContentRef。 */
export function buildRingingCommand(
  method: string,
  params: Record<string, unknown>,
): RingingCommandSpec | null {
  switch (method) {
    case "session.send_message": {
      const text = typeof params.text === "string" ? params.text : "";
      const images =
        Array.isArray(params.images) && params.images.length > 0
          ? (params.images as Array<{ mimeType: string; mime_type?: string; data: string }>).map(
              (img) => ({
                mime_type: img.mime_type ?? img.mimeType,
                data: img.data,
              }),
            )
          : undefined;
      if (!text) return null;
      return {
        channel: "conversation",
        command: {
          type: "conversation_send_message",
          text,
          ...(images && images.length > 0 ? { images } : {}),
        },
      };
    }
    case "session.cancel":
      return { channel: "conversation", command: { type: "conversation_cancel" } };
    case "session.compact":
      return { channel: "conversation", command: { type: "conversation_compact" } };
    default:
      return null;
  }
}

/** 命令请求：协议选择固定在连接级，Ringing V1 错误不再按命令回退 legacy。 */
export async function requestWithRinging<T>(
  method: string,
  params: Record<string, unknown> = {},
): Promise<T> {
  // Keep the historical helper name for callers, but let the connection-level
  // backend router choose the typed Ringing V1 route. This also ensures file paths reach main
  // for ContentRef upload instead of bypassing Electron's ownership boundary.
  return request<T>(method, params);
}
