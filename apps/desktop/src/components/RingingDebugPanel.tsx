// Ringing 调试面板（影子模式验证入口）。
//
// 功能：
// - 展示三频道 SSE 连接状态与每会话影子 store 概览；
// - 一键切流：选 seed + channel → prepare → commit → reload 页面。
//   （commit 后 legacy 停发该频道；reload 后 main 侧 sessionChannelMode
//   仍记着"已切流"，renderer 从 snapshot 重建——新实现真实接管渲染。）

import { createSignal, For, Show } from "solid-js";
import type { RingingMonitor } from "../store/ringingMonitor";
import type { ChannelName } from "../store/ringingMonitor";

export function RingingDebugPanel(props: { monitor: RingingMonitor }) {
  const { state, cutover, shadowOf } = props.monitor;
  const [seedInput, setSeedInput] = createSignal("");
  const [channel, setChannel] = createSignal<ChannelName>("tool");
  const [busy, setBusy] = createSignal(false);
  const [result, setResult] = createSignal<{ ok: boolean; text: string } | null>(null);

  const seeds = () => Object.keys(state().perSeed);

  async function doCutover(seed: string, ch: ChannelName): Promise<void> {
    setBusy(true);
    setResult(null);
    try {
      await cutover(seed, ch);
      setResult({ ok: true, text: `已切流 ${seed} / ${ch} → Ringing，即将刷新页面` });
      setTimeout(() => window.location.reload(), 800);
    } catch (error) {
      setResult({ ok: false, text: String(error) });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div
      style={{
        position: "fixed",
        right: "12px",
        bottom: "12px",
        "z-index": "9999",
        width: "340px",
        "max-height": "70vh",
        overflow: "auto",
        background: "rgba(24,24,28,0.96)",
        color: "#ddd",
        border: "1px solid #444",
        "border-radius": "8px",
        padding: "12px",
        "font-family": "monospace",
        "font-size": "12px",
        "box-shadow": "0 4px 24px rgba(0,0,0,0.5)",
      }}
    >
      <div style={{ display: "flex", "justify-content": "space-between", "margin-bottom": "8px" }}>
        <strong style={{ color: "#8fd3ff" }}>Ringing 调试面板</strong>
        <span style={{ color: "#7ee787" }}>● shadow</span>
      </div>

      <div style={{ "margin-bottom": "8px" }}>
        {(["control", "conversation", "tool"] as const).map((ch) => (
          <div>
            {ch}:{" "}
            <span style={{ color: state().channels[ch].state === "open" ? "#7ee787" : "#d29922" }}>
              {state().channels[ch].state}
            </span>
            <Show when={state().channels[ch].detail}>
              <span style={{ color: "#888" }}> ({state().channels[ch].detail})</span>
            </Show>
          </div>
        ))}
      </div>

      <Show when={state().lastError}>
        <div style={{ color: "#f85149", "margin-bottom": "8px" }}>⚠ {state().lastError}</div>
      </Show>

      <div style={{ "margin-bottom": "8px" }}>
        <div style={{ color: "#8fd3ff", "margin-bottom": "4px" }}>会话影子状态</div>
        <Show when={seeds().length > 0} fallback={<div style={{ color: "#888" }}>（尚无事件到达）</div>}>
          <For each={seeds()}>
            {(seed) => {
              const info = state().perSeed[seed];
              const shadow = shadowOf(seed);
              return (
                <div style={{ "border-bottom": "1px solid #333", padding: "4px 0" }}>
                  <div style={{ color: "#bbb" }}>{seed.slice(0, 8)}…</div>
                  <div style={{ color: "#888" }}>
                    事件 {info.applied} · turns {info.turns} · cards {info.toolCards}
                  </div>
                  <Show when={shadow && shadow.conversation.activeTurn}>
                    <div style={{ color: "#d29922" }}>
                      active: {shadow!.conversation.activeTurn!.turnId.slice(0, 8)}…{" "}
                      {shadow!.conversation.activeTurn!.status}
                    </div>
                  </Show>
                </div>
              );
            }}
          </For>
        </Show>
      </div>

      <div style={{ "border-top": "1px solid #444", "padding-top": "8px" }}>
        <div style={{ color: "#8fd3ff", "margin-bottom": "4px" }}>切流（sticky，commit 后不可回退）</div>
        <input
          value={seedInput()}
          onInput={(e) => setSeedInput(e.currentTarget.value)}
          placeholder="seed（留空=第一个活跃会话）"
          style={{
            width: "100%",
            background: "#111",
            color: "#ddd",
            border: "1px solid #444",
            "border-radius": "4px",
            padding: "4px",
            "margin-bottom": "4px",
          }}
        />
        <div style={{ display: "flex", gap: "4px", "margin-bottom": "4px" }}>
          {(["control", "conversation", "tool"] as const).map((ch) => (
            <button
              onClick={() => setChannel(ch)}
              style={{
                flex: "1",
                background: channel() === ch ? "#1f6feb" : "#222",
                color: "#ddd",
                border: "1px solid #444",
                "border-radius": "4px",
                padding: "4px",
                cursor: "pointer",
              }}
            >
              {ch}
            </button>
          ))}
        </div>
        <button
          disabled={busy()}
          onClick={() => {
            const seed = seedInput() || seeds()[0];
            if (!seed) {
              setResult({ ok: false, text: "没有活跃会话，先输入 seed" });
              return;
            }
            void doCutover(seed, channel());
          }}
          style={{
            width: "100%",
            background: "#1f6feb",
            color: "#fff",
            border: "none",
            "border-radius": "4px",
            padding: "6px",
            cursor: busy() ? "wait" : "pointer",
          }}
        >
          {busy() ? "切流中…" : `切流 ${channel()} → Ringing`}
        </button>
        <Show when={result()}>
          <div style={{ "margin-top": "4px", color: result()!.ok ? "#7ee787" : "#f85149" }}>
            {result()!.text}
          </div>
        </Show>
      </div>
    </div>
  );
}
