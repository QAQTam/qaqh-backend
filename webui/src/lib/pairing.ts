/**
 * 设备配对(daemon ringing v2)的壳命令封装:HTTP 与 admin token 都留在
 * Rust 侧,前端只拿 QR payload 字符串与设备列表(设备视图不含任何 token 材料)。
 * 非 Tauri 环境(浏览器 dev harness)不可用,调用方以 isTauriRuntime() 判定。
 */
import { invoke } from "@tauri-apps/api/core";
import { isTauriRuntime } from "./transport/backend";

export type PairingScope = "view" | "interact" | "admin";

export interface PairingTicket {
  /** 规范 §11 的 JSON 字符串(含 base_url/pairing_token/tls_fp),直接喂给二维码编码器。 */
  qr_payload: string;
  expires_in_ms: number;
  base_url: string;
}

export interface DeviceEntry {
  device_id: string;
  name: string;
  platform: string;
  scope: string;
  created_at_ms: number;
  last_seen_ms: number;
}

export function pairingCreate(
  scopeGrant: PairingScope,
  deviceName: string,
  platform?: string,
): Promise<PairingTicket> {
  if (!isTauriRuntime()) return Promise.reject(new Error("设备配对仅在桌面壳内可用"));
  return invoke<PairingTicket>("pairing_create", {
    scopeGrant,
    deviceName,
    platform: platform ?? null,
  });
}

export function devicesList(): Promise<{ devices: DeviceEntry[] }> {
  if (!isTauriRuntime()) return Promise.reject(new Error("设备配对仅在桌面壳内可用"));
  return invoke<{ devices: DeviceEntry[] }>("devices_list");
}

export function deviceRevoke(deviceId: string): Promise<void> {
  if (!isTauriRuntime()) return Promise.reject(new Error("设备配对仅在桌面壳内可用"));
  return invoke<void>("device_revoke", { deviceId });
}
