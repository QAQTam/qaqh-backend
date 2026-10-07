/**
 * 设备配对 section:桌面签发一次性配对令牌(120s)→ 渲染二维码 → 手机扫码自带
 * TLS pinning 直连 daemon;下方管理已配对设备(吊销会强制断开其 SSE)。
 *
 * 安全语义:二维码只在用户显式点击「生成」时签发——桌面确认 = 发 token。
 * admin token 与 HTTP 都留在 Rust 壳,webview 只见 payload 字符串与设备视图。
 */
import { createEffect, createSignal, For, onCleanup, Show, type Component } from "solid-js";
import IconQrCode from "~icons/lucide/qr-code";
import IconRefreshCw from "~icons/lucide/refresh-cw";
import { toast } from "../ui/toast";
import {
  deviceRevoke,
  devicesList,
  pairingCreate,
  type DeviceEntry,
  type PairingScope,
} from "../lib/pairing";
import { isTauriRuntime } from "../lib/transport";
import { Field, Section, TextInput } from "./controls";

const SCOPES: Array<{ id: PairingScope; label: string; desc: string }> = [
  { id: "view", label: "view", desc: "只读:看会话与输出" },
  { id: "interact", label: "interact", desc: "可交互:发送消息、响应授权" },
  { id: "admin", label: "admin", desc: "管理员:含设备管理(慎选)" },
];

const relativeTime = (ms: number): string => {
  const delta = Date.now() - ms;
  if (delta < 60_000) return "刚刚";
  if (delta < 3_600_000) return `${Math.floor(delta / 60_000)} 分钟前`;
  if (delta < 86_400_000) return `${Math.floor(delta / 3_600_000)} 小时前`;
  return `${Math.floor(delta / 86_400_000)} 天前`;
};

const messageOf = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause));

export const PairingSection: Component = () => {
  const available = isTauriRuntime();
  const [scope, setScope] = createSignal<PairingScope>("interact");
  const [deviceName, setDeviceName] = createSignal("");
  const [qrDataUrl, setQrDataUrl] = createSignal<string | null>(null);
  const [expiresAt, setExpiresAt] = createSignal(0);
  const [now, setNow] = createSignal(Date.now());
  const [creating, setCreating] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [devices, setDevices] = createSignal<DeviceEntry[] | null>(null);
  const [devicesLoading, setDevicesLoading] = createSignal(false);
  const [confirmRevoke, setConfirmRevoke] = createSignal<string | null>(null);

  // 倒计时只在有活码时走:250ms 一跳只为进度条平滑,秒数文本按秒变。
  createEffect(
    () => [qrDataUrl() != null, expiresAt()],
    ([live]) => {
      if (!live) return;
      const timer = window.setInterval(() => setNow(Date.now()), 250);
      onCleanup(() => window.clearInterval(timer));
    },
  );

  const loadDevices = async (): Promise<void> => {
    setDevicesLoading(true);
    try {
      setDevices((await devicesList()).devices);
    } catch (cause) {
      setError(messageOf(cause));
    } finally {
      setDevicesLoading(false);
    }
  };

  // 设备列表进设置就拉一次(available 恒定 → effect 只跑一次)。
  createEffect(
    () => available,
    () => {
      if (available) void loadDevices();
    },
  );

  const expired = (): boolean => expiresAt() > 0 && now() >= expiresAt();
  const remainingSec = (): number => Math.max(0, Math.ceil((expiresAt() - now()) / 1000));

  const generate = async (): Promise<void> => {
    setCreating(true);
    setError(null);
    try {
      const [{ default: QRCode }, ticket] = await Promise.all([
        import("qrcode"),
        pairingCreate(scope(), deviceName().trim() || "移动设备"),
      ]);
      const dataUrl = await QRCode.toDataURL(ticket.qr_payload, { margin: 1, width: 220 });
      setQrDataUrl(dataUrl);
      setExpiresAt(Date.now() + ticket.expires_in_ms);
      setNow(Date.now());
    } catch (cause) {
      setQrDataUrl(null);
      setExpiresAt(0);
      setError(messageOf(cause));
    } finally {
      setCreating(false);
    }
  };

  const revoke = async (deviceId: string): Promise<void> => {
    try {
      await deviceRevoke(deviceId);
      setConfirmRevoke(null);
      toast("设备已吊销", "ok");
      void loadDevices();
    } catch (cause) {
      toast(messageOf(cause), "err");
    }
  };

  return (
    <Section
      id="section-pairing"
      title="设备配对(移动端)"
      desc="生成一次性二维码,用移动端扫码绑定本机 daemon。令牌 120 秒有效且只能用一次。"
    >      <Show
        when={available}
        fallback={<p class="field-hint" style={{ "grid-column": "1 / -1" }}>浏览器 dev 环境不可用:请在桌面壳内使用。</p>}
      >
        <Field label="授予档位" wide>
          <div class="scope-list">
            <For each={SCOPES}>
              {(item) => (
                <label class={`scope${scope() === item.id ? " active" : ""}${item.id === "admin" ? " dangerous" : ""}`}>
                  <input
                    type="radio"
                    name="pairing-scope"
                    checked={scope() === item.id}
                    onInput={() => setScope(item.id)}
                  />
                  <span class="scope-name">{item.label}</span>
                  <span class="scope-desc">{item.desc}</span>
                </label>
              )}
            </For>
          </div>
        </Field>
        <Field label="设备名" hint="给手机起个名字,便于在设备列表里辨认。">
          <TextInput value={deviceName()} onInput={setDeviceName} placeholder="我的手机" />
        </Field>
        <div class="pairing-actions">
          <button type="button" class="ghost-mini" disabled={creating()} onClick={() => void generate()}>
            <Show when={qrDataUrl() != null} fallback={<IconQrCode />}>
              <IconRefreshCw />
            </Show>
            {creating() ? "生成中…" : qrDataUrl() == null ? "生成配对二维码" : expired() ? "已过期,重新生成" : "重新生成"}
          </button>
          <Show when={qrDataUrl() != null && !expired()}>
            <span class="pairing-count">{remainingSec()}s 后过期</span>
          </Show>
        </div>
        <Show when={error() != null}>
          <p class="field-hint is-error" style={{ "grid-column": "1 / -1" }}>{error()}</p>
        </Show>
        <Show when={qrDataUrl() != null}>
          <div class="pairing-qr" style={{ "grid-column": "1 / -1" }}>
            <img src={qrDataUrl()!} alt="配对二维码" width={220} height={220} />
            <div class="pairing-qr-note">
              <Show
                when={!expired()}
                fallback={<span class="pairing-expired">二维码已过期,请重新生成。</span>}
              >
                <span>
                  用移动端扫码绑定。令牌一次性,{remainingSec()}s 后过期;过 TLS 指纹校验直连本机 daemon。
                </span>
              </Show>
            </div>
          </div>
        </Show>

        <div class="devices-block" style={{ "grid-column": "1 / -1" }}>
          <div class="devices-head">
            <span>已配对设备</span>
            <button type="button" class="ghost-mini" disabled={devicesLoading()} onClick={() => void loadDevices()}>
              <IconRefreshCw class={{ spin: devicesLoading() }} />
              刷新
            </button>
          </div>
          <Show
            when={devices() != null && devices()!.length > 0}
            fallback={<p class="field-hint">{devicesLoading() ? "加载中…" : "暂无已配对设备。"}</p>}
          >
            <table class="readonly-table devices-table">
              <thead>
                <tr>
                  <th>名称</th>
                  <th>平台</th>
                  <th>档位</th>
                  <th>最近活跃</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                <For each={devices()}>
                  {(device) => (
                    <tr>
                      <td>{device.name}</td>
                      <td>{device.platform}</td>
                      <td><span class={`scope-badge ${device.scope}`}>{device.scope}</span></td>
                      <td>{relativeTime(device.last_seen_ms)}</td>
                      <td class="devices-actions">
                        <Show
                          when={confirmRevoke() === device.device_id}
                          fallback={
                            <button type="button" class="ghost-mini is-danger" onClick={() => setConfirmRevoke(device.device_id)}>
                              吊销
                            </button>
                          }
                        >
                          <span class="devices-confirm">
                            确认吊销?
                            <button type="button" class="ghost-mini is-danger" onClick={() => void revoke(device.device_id)}>
                              确认
                            </button>
                            <button type="button" class="ghost-mini" onClick={() => setConfirmRevoke(null)}>
                              取消
                            </button>
                          </span>
                        </Show>
                      </td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </Show>
        </div>
      </Show>
    </Section>
  );
};
