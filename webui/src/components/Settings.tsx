/**
 * 设置页（v1）：完全构建在后端已有服务面上。
 *
 * 读 = `config.load`（ConfigDto，camelCase）；写 = `config.save`（ConfigPatch，
 * JSON Merge Patch —— 只提交变化过的字段，缺失 = 不动，天然避免整包写回毒化）。
 * 权限档位在 patch 内有 1..=4 值域校验；apiKey 沿用守卫："****"/空串 = 保持现值。
 */
import { createSignal, createStore, For, Show, type Component } from "solid-js";
import { ringing } from "../lib/ringing";
import { seed } from "../state";

type Dto = Record<string, any>;

const EFFORTS = ["off", "low", "medium", "high", "max"];
const PERMISSIONS = [
  { level: 1, name: "MaxLockdown", desc: "一切工具调用都需确认" },
  { level: 2, name: "ReadFree", desc: "读放行，写/exec/net 需确认" },
  { level: 3, name: "WorkspaceFree", desc: "工作区内写放行；跨区一次性信任；exec/net 仍确认" },
  { level: 4, name: "Unrestricted", desc: "显式危险 bypass：普通工具全部自动放行" },
];
const PATCH_KEYS = [
  "providerId", "endpoint", "baseUrl", "model", "maxTokens", "contextLimit",
  "reasoningEffort", "autoCompactThreshold", "permissionLevel", "theme", "lang",
  "fontFamily", "notificationsEnabled",
];

export const Settings: Component<{ onClose: () => void }> = (props) => {
  const [draft, setDraft] = createStore<Dto>({});
  const [loaded, setLoaded] = createSignal(false);
  const [baseline, setBaseline] = createSignal<Dto | null>(null);
  const [busy, setBusy] = createSignal(false);
  const [toast, setToast] = createSignal<{ text: string; error: boolean } | null>(null);
  const [newProfile, setNewProfile] = createSignal("");
  const [apiKeyInput, setApiKeyInput] = createSignal("");
  const [toolMode, setToolMode] = createSignal("standard");

  const flash = (text: string, error = false) => {
    setToast({ text, error });
    setTimeout(() => setToast(null), 2600);
  };

  const load = async () => {
    const dto = await ringing.rpc<Dto>("config.load");
    setBaseline(structuredClone(dto) as Dto);
    // Solid 2 store setter takes a function only; returning a value performs a
    // top-level shallow replacement — exactly right for a fresh DTO load.
    setDraft(() => structuredClone(dto) as Dto);
    setLoaded(true);
  };
  void load().catch((e) => flash(String(e instanceof Error ? e.message : e), true));

  const dirtyKeys = () => {
    const base = baseline();
    if (!base || !loaded()) return [];
    return PATCH_KEYS.filter((key) => String(draft[key]) !== String(base[key]));
  };
  const isDirty = () => dirtyKeys().length > 0 || (apiKeyInput() !== "" && apiKeyInput() !== "****");

  const buildPatch = (): Record<string, unknown> => {
    const patch: Record<string, unknown> = {};
    for (const key of dirtyKeys()) {
      const value = draft[key];
      if (key === "maxTokens" || key === "contextLimit") patch[key] = Number(value);
      else if (key === "autoCompactThreshold") patch[key] = Number(value);
      else if (key === "permissionLevel") patch[key] = Number(value);
      else if (key === "notificationsEnabled") patch[key] = value === true;
      else patch[key] = value;
    }
    const key = apiKeyInput().trim();
    if (key && key !== "****") patch.apiKey = key;
    return patch;
  };

  const save = async () => {
    const patch = buildPatch();
    if (Object.keys(patch).length === 0) {
      flash("没有变更");
      return;
    }
    setBusy(true);
    try {
      await ringing.rpc("config.save", patch);
      await load();
      setApiKeyInput("");
      flash(`已保存 ${Object.keys(patch).length} 项（worker 将自动重载配置）`);
    } catch (e) {
      flash(String(e instanceof Error ? e.message : e), true);
    } finally {
      setBusy(false);
    }
  };

  const setPermission = async (level: number) => {
    setDraft((d) => { d.permissionLevel = level; });
  };

  const provider = () => (draft.providers ?? []).find((p: any) => p.id === draft.providerId);
  const endpoint = () => provider()?.endpoints?.find((e: any) => e.id === draft.endpoint);

  const pickProvider = (id: string) => {
    setDraft((d) => { d.providerId = id; });
    const next = (draft.providers ?? []).find((p: any) => p.id === id);
    const first = next?.endpoints?.[0];
    if (first) {
      setDraft((d) => { d.endpoint = first.id; });
      setDraft((d) => { d.baseUrl = first.baseUrl; });
      if (first.defaultModel) setDraft((d) => { d.model = first.defaultModel; });
    }
  };
  const pickEndpoint = (id: string) => {
    setDraft((d) => { d.endpoint = id; });
    const next = provider()?.endpoints?.find((e: any) => e.id === id);
    if (next) {
      setDraft((d) => { d.baseUrl = next.baseUrl; });
      if (next.defaultModel) setDraft((d) => { d.model = next.defaultModel; });
    }
  };

  const modelOptions = () => {
    const list = [...(endpoint()?.models ?? [])];
    const current = draft.model;
    if (current && !list.includes(current)) list.unshift(current);
    return list;
  };

  const applyProfile = async (name: string) => {
    setBusy(true);
    try {
      await ringing.rpc("profile.apply", { name });
      await load();
      flash(`已应用档案 ${name}`);
    } catch (e) {
      flash(String(e instanceof Error ? e.message : e), true);
    } finally {
      setBusy(false);
    }
  };
  const saveCurrentProfile = async () => {
    const name = newProfile().trim();
    if (!name) return;
    setBusy(true);
    try {
      await ringing.rpc("profile.save_current", { name });
      setNewProfile("");
      await load();
      flash(`已保存档案 ${name}`);
    } catch (e) {
      flash(String(e instanceof Error ? e.message : e), true);
    } finally {
      setBusy(false);
    }
  };
  const deleteProfile = async (name: string) => {
    setBusy(true);
    try {
      await ringing.rpc("profile.delete", { name });
      await load();
      flash(`已删除档案 ${name}`);
    } catch (e) {
      flash(String(e instanceof Error ? e.message : e), true);
    } finally {
      setBusy(false);
    }
  };

  const applyToolMode = async (mode: string) => {
    setBusy(true);
    try {
      await ringing.command("control", { channel: "control", type: "set_tool_mode", tool_mode: mode }, seed() ?? undefined);
      setToolMode(mode);
      flash(`当前会话工具模式 → ${mode}`);
    } catch (e) {
      flash(String(e instanceof Error ? e.message : e), true);
    } finally {
      setBusy(false);
    }
  };

  return (
    <div class="modal-overlay" onClick={props.onClose}>
      <div class="modal" onClick={(e) => e.stopPropagation()}>
        <header class="modal-head">
          <h2>设置</h2>
          <span class="meta">{isDirty() ? `未保存变更：${dirtyKeys().length} 项` : "与 daemon 配置一致"}</span>
          <button class="modal-close" onClick={props.onClose}>×</button>
        </header>

        <Show when={draft} fallback={<div class="modal-body"><p class="meta">加载配置…</p></div>}>
          <div class="modal-body">
            <section>
              <h3>模型与接入</h3>
              <div class="field">
                <label>供应商</label>
                <select value={draft.providerId} onChange={(e) => pickProvider(e.currentTarget.value)}>
                  <For each={draft.providers ?? []}>{(p: any) => <option value={p.id}>{p.display || p.id}</option>}</For>
                </select>
              </div>
              <div class="field">
                <label>接入点</label>
                <select value={draft.endpoint} onChange={(e) => pickEndpoint(e.currentTarget.value)}>
                  <For each={provider()?.endpoints ?? []}>{(e: any) => <option value={e.id}>{e.display || e.id}</option>}</For>
                </select>
              </div>
              <div class="field">
                <label>模型</label>
                <select value={draft.model} onChange={(e) => setDraft((d) => { d.model = e.currentTarget.value; })}>
                  <For each={modelOptions()}>{(m: string) => <option value={m}>{m}</option>}</For>
                </select>
              </div>
              <div class="field">
                <label>Base URL</label>
                <input type="text" value={draft.baseUrl} onInput={(e) => setDraft((d) => { d.baseUrl = e.currentTarget.value; })} />
              </div>
              <div class="field">
                <label>API Key（留空 = 保持不变）</label>
                <input type="password" placeholder={draft.apiKey || "未配置"} value={apiKeyInput()} onInput={(e) => setApiKeyInput(e.currentTarget.value)} />
              </div>
            </section>

            <section>
              <h3>生成参数</h3>
              <div class="field">
                <label>推理力度</label>
                <select value={draft.reasoningEffort} onChange={(e) => setDraft((d) => { d.reasoningEffort = e.currentTarget.value; })}>
                  <For each={EFFORTS}>{(v) => <option value={v}>{v}</option>}</For>
                </select>
              </div>
              <div class="field-row">
                <div class="field">
                  <label>最大输出 tokens</label>
                  <input type="number" value={draft.maxTokens} onInput={(e) => setDraft((d) => { d.maxTokens = Number(e.currentTarget.value); })} />
                </div>
                <div class="field">
                  <label>上下文上限</label>
                  <input type="number" value={draft.contextLimit} onInput={(e) => setDraft((d) => { d.contextLimit = Number(e.currentTarget.value); })} />
                </div>
              </div>
              <div class="field">
                <label>自动压缩阈值（0 = 关闭）</label>
                <input
                  type="range" min="0" max="1" step="0.05"
                  value={draft.autoCompactThreshold}
                  onInput={(e) => setDraft((d) => { d.autoCompactThreshold = Number(e.currentTarget.value); })}
                />
                <span class="meta">{Number(draft.autoCompactThreshold).toFixed(2)}</span>
              </div>
            </section>

            <section>
              <h3>权限档位</h3>
              <For each={PERMISSIONS}>
                {(p) => (
                  <label class="perm-option">
                    <input
                      type="radio" name="perm" checked={Number(draft.permissionLevel) === p.level}
                      onChange={() => void setPermission(p.level)}
                    />
                    <span>
                      <b>L{p.level} {p.name}</b> — {p.desc}
                      {p.level === 4 ? <em class="danger-text">（危险）</em> : ""}
                    </span>
                  </label>
                )}
              </For>
            </section>

            <section>
              <h3>界面</h3>
              <div class="field">
                <label>主题</label>
                <select value={draft.theme ?? ""} onChange={(e) => setDraft((d) => { d.theme = e.currentTarget.value; })}>
                  <option value="">跟随系统</option>
                  <option value="dark">暗色</option>
                  <option value="light">亮色</option>
                </select>
              </div>
              <div class="field">
                <label>字体族</label>
                <input type="text" value={draft.fontFamily ?? ""} onInput={(e) => setDraft((d) => { d.fontFamily = e.currentTarget.value; })} />
              </div>
              <div class="field">
                <label>语言</label>
                <input type="text" value={draft.lang ?? ""} onInput={(e) => setDraft((d) => { d.lang = e.currentTarget.value; })} />
              </div>
              <label class="perm-option">
                <input
                  type="checkbox" checked={draft.notificationsEnabled === true}
                  onChange={(e) => setDraft((d) => { d.notificationsEnabled = e.currentTarget.checked; })}
                />
                <span>桌面通知</span>
              </label>
            </section>

            <section>
              <h3>当前会话工具模式</h3>
              <div class="actions">
                <For each={["standard", "minimal", "custom"]}>
                  {(mode) => (
                    <button disabled={busy()} onClick={() => void applyToolMode(mode)}>
                      {mode}{toolMode() === mode ? " ✓" : ""}
                    </button>
                  )}
                </For>
              </div>
            </section>

            <section>
              <h3>配置档案</h3>
              <div class="meta">当前：{draft.activeProfile || "（默认）"}</div>
              <div class="field-row">
                <div class="field grow">
                  <input type="text" placeholder="档案名…" value={newProfile()} onInput={(e) => setNewProfile(e.currentTarget.value)} />
                </div>
                <button disabled={busy() || !newProfile().trim()} onClick={() => void saveCurrentProfile()}>保存当前为档案</button>
              </div>
              <For each={draft.profiles ?? []}>
                {(name: string) => (
                  <div class="profile-row">
                    <span>{name}{name === draft.activeProfile ? "（当前）" : ""}</span>
                    <span class="actions">
                      <button disabled={busy()} onClick={() => void applyProfile(name)}>应用</button>
                      <Show when={name !== "default"}>
                        <button class="danger" disabled={busy()} onClick={() => void deleteProfile(name)}>删除</button>
                      </Show>
                    </span>
                  </div>
                )}
              </For>
            </section>
          </div>
        </Show>

        <footer class="modal-foot">
          <span class="meta">{toast() ? toast()!.text : ""}</span>
          <span class="spacer" />
          <button onClick={props.onClose}>关闭</button>
          <button class="primary" disabled={busy() || !isDirty()} onClick={() => void save()}>
            保存{dirtyKeys().length > 0 ? `（${dirtyKeys().length} 项）` : ""}
          </button>
        </footer>
      </div>
    </div>
  );
};
