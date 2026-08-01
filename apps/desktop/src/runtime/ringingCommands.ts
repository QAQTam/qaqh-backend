// Ringing 命令/查询 renderer 入口。
//
// localStorage["ringing.commands"] === "1" 时，session.send_message /
// session.cancel / session.compact 走 Ringing HTTP 命令端点；任何失败
// 自动回退 legacy `request()`（记录日志，不阻塞 UI）。
//
// 除全局开关外，还支持每 (seed, channel) 的命令切流（commandProtocol=ringing，
// 见 ringingCommandRouter）：该模式下 send/cancel/compact 同样经 Ringing，
// 但只有 "ringing not connected" 才回退 legacy，其它错误原样抛出（sticky）。
//
// 边界：
// - 带 files 的 send_message 保持 legacy——文件预览展开在 daemon 侧读文件
//   （service.rs with_file_previews），renderer 沙箱内无法复刻。
// - ack 只表示 accepted；业务结果经 Ringing 事件流（causation_id = command_id）
//   返回，本 helper 不等待终态。
// - 失败回退 legacy 时，若 Ringing 侧实际已 accepted（网络歧义），可能造成
//   重复执行；命令端点幂等键在 Ringing 内部，legacy 重试无法复用，这是
//   opt-in 调试开关的已知权衡（cancel/compact 天然幂等，风险集中在 send）。

import type { RingingChannel } from "../lib/types/ringing";
import { requestLegacy } from "./backendClient";
import { commandIsRinging } from "./ringingCommandRouter";

const RINGING_COMMANDS_KEY = "ringing.commands";

export function ringingCommandsEnabled(): boolean {
  try {
    return localStorage.getItem(RINGING_COMMANDS_KEY) === "1";
  } catch {
    return false;
  }
}

interface RingingCommandSpec {
  channel: RingingChannel;
  command: unknown;
}

/** legacy 方法 → Ringing 频道命令映射；不支持的场景返回 null（保持 legacy）。 */
export function buildRingingCommand(
  method: string,
  params: Record<string, unknown>,
): RingingCommandSpec | null {
  switch (method) {
    case "session.send_message": {
      const text = typeof params.text === "string" ? params.text : "";
      const files = Array.isArray(params.files) ? params.files : [];
      const images =
        Array.isArray(params.images) && params.images.length > 0
          ? (params.images as Array<{ mimeType: string; mime_type?: string; data: string }>).map(
              (img) => ({
                mime_type: img.mime_type ?? img.mimeType,
                data: img.data,
              }),
            )
          : undefined;
      if (!text || files.length > 0) return null;
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

/** 命令请求（带 legacy 回退）：Ringing 可用则走新协议，否则/失败回退 legacy。 */
export async function requestWithRinging<T>(
  method: string,
  params: Record<string, unknown> = {},
): Promise<T> {
  const bridge = window.deepx?.ringing;
  const seed = typeof params.seed === "string" ? params.seed : "";
  const force = ringingCommandsEnabled();
  const spec = buildRingingCommand(method, params);
  if (spec && bridge && (force || commandIsRinging(seed, spec.channel))) {
    try {
      const ack = await bridge.command(seed, spec.channel, {
        command_id: commandId(),
        command: spec.command,
        seed,
      });
      const status = (ack as { status?: string; code?: string; message?: string } | null)?.status;
      if (status === "rejected") {
        const code = (ack as { code?: string } | null)?.code ?? "rejected";
        throw new Error(`Ringing rejected command: ${code}`);
      }
      return undefined as T;
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      if (force || message.includes("ringing not connected")) {
        console.warn(`[ringing] ${method} via Ringing failed, falling back to legacy`, error);
      } else {
        throw error;
      }
    }
  }
  return requestLegacy<T>(method, params);
}

let commandCounter = 0;
function commandId(): string {
  commandCounter += 1;
  if (typeof crypto !== "undefined" && typeof crypto.randomUUID === "function") {
    return crypto.randomUUID();
  }
  return `cmd-${Date.now()}-${commandCounter}`;
}
