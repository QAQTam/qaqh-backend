import { createEffect, createSignal, For, onSettled, Show } from "solid-js";
import { request } from "../runtime/backendClient";
import { confirmDialog, openDialog } from "../runtime/desktopApi";
import { useI18n, type Lang } from "../i18n";
import PermissionLevelSelect from "./composer/PermissionLevelSelect";

export type ThemeMode = "system" | "light" | "dark" | "dark-gray";

interface Provider { id: string; display: string; endpoints: Endpoint[]; }
interface Endpoint { id: string; display: string; base_url: string; default_model: string; models: string[]; stateful?: boolean; }

interface SettingsViewProps {
  lang: () => Lang; onLangChange: (l: Lang) => void;
  theme: () => ThemeMode; onThemeChange: (t: ThemeMode) => void;
  permissionLevel: number;
  onPermissionLevelChange: (level: number) => void | Promise<void>;
}

interface Category {
  id: string;
  label: string;
}

function EyeIcon(props: { show: boolean }) {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
      {props.show
        ? <><path d="M17.94 17.94A10.07 10.07 0 0112 20c-7 0-11-8-11-8a18.45 18.45 0 015.06-5.94M9.9 4.24A9.12 9.12 0 0112 4c7 0 11 8 11 8a18.5 18.5 0 01-2.16 3.19m-6.72-1.07a3 3 0 11-4.24-4.24"/><line x1="1" y1="1" x2="23" y2="23"/></>
        : <><path d="M1 12s4-8 11-8 11 8 11 8-4 8-11 8-11-8-11-8z"/><circle cx="12" cy="12" r="3"/></>
      }
    </svg>
  );
}

function SecretInput(props: {
  configured: boolean;
  showInput: boolean;
  value: string;
  onInput: (v: string) => void;
  onReplace: () => void;
  onCancel: () => void;
  placeholder: string;
  configuredLabel: string;
  replaceLabel: string;
  cancelLabel: string;
  hint: string;
}) {
  return (
    <div class="settings-input-group">
      <Show
        when={props.configured && !props.showInput}
        fallback={
          <div>
            <div class="settings-secret-row">
              <input
                type="password"
                value={props.value}
                onInput={(e) => props.onInput(e.currentTarget.value)}
                placeholder={props.placeholder}
                autocomplete="off"
              />
              <Show when={props.configured}>
                <button
                  class="settings-link-btn"
                  onClick={props.onCancel}
                >
                  {props.cancelLabel}
                </button>
              </Show>
            </div>
            <div class="settings-hint">{props.hint}</div>
          </div>
        }
      >
        <div>
          <div class="settings-secret-row">
            <span class="settings-configured-badge">{props.configuredLabel}</span>
            <button class="settings-link-btn" onClick={props.onReplace}>
              {props.replaceLabel}
            </button>
          </div>
          <div class="settings-hint">{props.hint}</div>
        </div>
      </Show>
    </div>
  );
}

export default function SettingsView(props: SettingsViewProps) {
  const { t } = useI18n();
  const [apiKeyValue, setApiKeyValue] = createSignal("");
  const [apiKeyConfigured, setApiKeyConfigured] = createSignal(false);
  const [showApiKeyInput, setShowApiKeyInput] = createSignal(false);

  const [model, setModel] = createSignal("");
  const [baseUrl, setBaseUrl] = createSignal("");
  const [providerId, setProviderId] = createSignal("deepseek");
  const [endpointId, setEndpointId] = createSignal("openai");
  const [maxTokens, setMaxTokens] = createSignal(16384);
  const [contextLimit, setContextLimit] = createSignal(1000000);
  const [reasoningEffort, setReasoningEffort] = createSignal("high");
  const [autoCompactThreshold, setAutoCompactThreshold] = createSignal(0.75);
  const [autoCompactEnabled, setAutoCompactEnabled] = createSignal(true);
  const [complianceEnabled, setComplianceEnabled] = createSignal(true);
  const [databaseEnabled, setDatabaseEnabled] = createSignal(true);
  const [migrationPending, setMigrationPending] = createSignal(0);
  const [migrating, setMigrating] = createSignal(false);
  const [migrationResult, setMigrationResult] = createSignal("");
  const [migrationFailed, setMigrationFailed] = createSignal(false);
  const [migrationProgress, setMigrationProgress] = createSignal(0);
  const [migrationPhase, setMigrationPhase] = createSignal<"idle" | "confirm" | "running" | "done">("idle");
  const [dualWriteChecked, setDualWriteChecked] = createSignal(true);
  const [saved, setSaved] = createSignal(false);
  let dbToggled = false;

  // Subagent
  const [subModel, setSubModel] = createSignal("");
  const [subBaseUrl, setSubBaseUrl] = createSignal("");
  const [subApiKeyValue, setSubApiKeyValue] = createSignal("");
  const [subApiKeyConfigured, setSubApiKeyConfigured] = createSignal(false);
  const [showSubApiKeyInput, setShowSubApiKeyInput] = createSignal(false);
  const [subMaxTokens, setSubMaxTokens] = createSignal(4096);
  const [subTimeout, setSubTimeout] = createSignal(120);
  const [subTools, setSubTools] = createSignal<string[]>(["read_file", "search", "grep", "exec", "list_dir", "glob"]);
  const [tokenizerPath, setTokenizerPath] = createSignal("");

  const [activeCategory, setActiveCategory] = createSignal("models");

  // -- Multimodal config --
  const [mmEnabled, setMmEnabled] = createSignal(false);
  const [mmApiKeyValue, setMmApiKeyValue] = createSignal("");
  const [mmApiKeyConfigured, setMmApiKeyConfigured] = createSignal(false);
  const [showMmApiKeyInput, setShowMmApiKeyInput] = createSignal(false);
  const [mmBaseUrl, setMmBaseUrl] = createSignal("");
  const [mmModel, setMmModel] = createSignal("mimo-v2.5");
  const [mmProviderType, setMmProviderType] = createSignal("mimo");
  const [mmMaxTokens, setMmMaxTokens] = createSignal(4096);

  const [configData, setConfigData] = createSignal<any>(null);
  const [configLoading, setConfigLoading] = createSignal(true);
  const [configError, setConfigError] = createSignal<any>(null);

  const refetchConfig = async () => {
    setConfigLoading(true);
    setConfigError(null);
    try {
      setConfigData(await request<unknown>("config.load"));
    } catch (e) {
      console.error(e);
      setConfigError(e);
    } finally {
      setConfigLoading(false);
    }
  };

  onSettled(() => { void refetchConfig(); });

  const [allTools, setAllTools] = createSignal<string[]>([]);
  onSettled(() => {
    void (async () => {
    try {
      setAllTools(await request<string[]>("skills.list_tools"));
    } catch (e) { console.error(e); }
    })();
  });

  const [loadError, setLoadError] = createSignal<string | null>(null);

  createEffect(
    () => ({ data: configData(), loading: configLoading(), error: configError() }),
    ({ data, loading, error }) => {
    if (loading) return;
    if (error) {
      setLoadError(String(error));
      return;
    }
    if (!data) {
      setLoadError("No config data received");
      return;
    }
    setLoadError(null);

    if (data.api_key) {
      const isMasked = data.api_key === "****";
      setApiKeyConfigured(isMasked);
      setApiKeyValue(isMasked ? "" : data.api_key);
      setShowApiKeyInput(!isMasked);
    }
    if (data.model) setModel(data.model);
    if (data.base_url) setBaseUrl(data.base_url);
    if (data.provider_id) setProviderId(data.provider_id);
    if (data.endpoint) setEndpointId(data.endpoint);
    if (data.max_tokens) setMaxTokens(data.max_tokens);
    if (data.context_limit) setContextLimit(data.context_limit);
    if (data.reasoning_effort) setReasoningEffort(data.reasoning_effort);
    if (data.auto_compact_threshold !== undefined) {
      setAutoCompactThreshold(data.auto_compact_threshold);
      setAutoCompactEnabled(data.auto_compact_threshold > 0);
    }
    if (data.compliance_enabled !== undefined) setComplianceEnabled(data.compliance_enabled);
    if (data.database?.enabled !== undefined) setDatabaseEnabled(data.database.enabled);
    if (data.subagent) {
      if (data.subagent.model) setSubModel(data.subagent.model);
      if (data.subagent.base_url) setSubBaseUrl(data.subagent.base_url);
      if (data.subagent.api_key) {
        const isMasked = data.subagent.api_key === "****";
        setSubApiKeyConfigured(isMasked);
        setSubApiKeyValue(isMasked ? "" : data.subagent.api_key);
        setShowSubApiKeyInput(!isMasked);
      }
      if (data.subagent.max_tokens) setSubMaxTokens(data.subagent.max_tokens);
      if (data.subagent.timeout_secs) setSubTimeout(data.subagent.timeout_secs);
      if (data.subagent.default_tools?.length) setSubTools(data.subagent.default_tools);
    }
    if (data.multimodal) {
      if (data.multimodal.enabled !== undefined) setMmEnabled(data.multimodal.enabled);
      if (data.multimodal.provider_type) setMmProviderType(data.multimodal.provider_type);
      if (data.multimodal.api_key) {
        const isMasked = data.multimodal.api_key === "****";
        setMmApiKeyConfigured(isMasked);
        setMmApiKeyValue(isMasked ? "" : data.multimodal.api_key);
        setShowMmApiKeyInput(!isMasked);
      }
      if (data.multimodal.base_url) setMmBaseUrl(data.multimodal.base_url);
      if (data.multimodal.model) setMmModel(data.multimodal.model);
      if (data.multimodal.max_tokens) setMmMaxTokens(data.multimodal.max_tokens);
    }
  });

  const providers = (): Provider[] => configData()?.providers ?? [];
  const currentEndpoints = (): Endpoint[] => {
    const p = providers().find((p: Provider) => p.id === providerId());
    return p?.endpoints ?? [];
  };
  const currentModels = (): string[] => {
    const ep = currentEndpoints().find((e: Endpoint) => e.id === endpointId());
    return ep?.models ?? [];
  };

  function handleProviderChange(id: string) {
    setProviderId(id);
    const ep = providers().find((p: Provider) => p.id === id)?.endpoints[0];
    if (ep) { setEndpointId(ep.id); setBaseUrl(ep.base_url); setModel(ep.default_model); }
  }
  function handleEndpointChange(id: string) {
    setEndpointId(id);
    const ep = currentEndpoints().find((e: Endpoint) => e.id === id);
    if (ep) { setBaseUrl(ep.base_url); setModel(ep.default_model); }
  }
  async function browseTokenizer() {
    const selected = await openDialog({});
    if (selected && typeof selected === 'string') setTokenizerPath(selected);
  }
  function toggleTool(name: string) {
    setSubTools(prev => prev.includes(name) ? prev.filter(t => t !== name) : [...prev, name]);
  }

  async function toggleDatabase(enabled: boolean) {
    try {
      await request("config.set_database_enabled", { enabled });
      setDatabaseEnabled(enabled);
      dbToggled = false;
      setSaved(true);
      setTimeout(() => setSaved(false), 2500);
    } catch (e) {
      console.error(e);
    }
  }

  async function save() {
    try {
      const apiKeyReplacement = !apiKeyConfigured() || showApiKeyInput() ? apiKeyValue() : "";
      const subApiKeyReplacement = !subApiKeyConfigured() || showSubApiKeyInput() ? subApiKeyValue() : "";
      const mmApiKeyReplacement = !mmApiKeyConfigured() || showMmApiKeyInput() ? mmApiKeyValue() : "";
      await request("config.save", {
        apiKey: apiKeyReplacement,
        model: model(), baseUrl: baseUrl(),
        providerId: providerId(), endpoint: endpointId(),
        maxTokens: maxTokens(), contextLimit: contextLimit(),
        reasoningEffort: reasoningEffort(), autoCompactThreshold: autoCompactEnabled() ? autoCompactThreshold() : 0.0, lang: props.lang(),
        subagentModel: subModel(), subagentBaseUrl: subBaseUrl(),
        subagentApiKey: subApiKeyReplacement,
        subagentMaxTokens: subMaxTokens(),
        subagentTimeoutSecs: subTimeout(), subagentDefaultTools: subTools(),
        databaseEnabled: databaseEnabled(),
        tokenizerPath: tokenizerPath(),
        multimodalProviderType: mmProviderType(),
        multimodalEnabled: mmEnabled(),
        multimodalApiKey: mmApiKeyReplacement, multimodalBaseUrl: mmBaseUrl(),
        multimodalModel: mmModel(), multimodalMaxTokens: mmMaxTokens(),
      });
      if (apiKeyConfigured() || apiKeyReplacement) {
        setApiKeyConfigured(true);
        setApiKeyValue("");
        setShowApiKeyInput(false);
      }
      if (subApiKeyConfigured() || subApiKeyReplacement) {
        setSubApiKeyConfigured(true);
        setSubApiKeyValue("");
        setShowSubApiKeyInput(false);
      if (mmApiKeyConfigured() || mmApiKeyReplacement) {
        setMmApiKeyConfigured(true);
        setMmApiKeyValue("");
        setShowMmApiKeyInput(false);
      }
      }
      setSaved(true);
      setTimeout(() => setSaved(false), 2500);
      if (dbToggled) {
        dbToggled = false;
        await confirmDialog(t().settings.restartPrompt, {
          title: t().settings.restartTitle,
          kind: "info",
        });
      }
    } catch (e) { console.error(e); }
  }

  // ── Migration ──
  createEffect(
    () => databaseEnabled(),
    enabled => {
    void (async () => {
    if (!enabled) return;
    try {
      const data = await request<{ pending?: number }>("config.database_migration_count");
      setMigrationPending(data.pending ?? 0);
    } catch (_) { setMigrationPending(0); }
    })();
  });

  function startMigrate() {
    if (migrationPending() === 0) return;
    setMigrationPhase("confirm");
  }

  async function doMigrate() {
    setMigrationPhase("running");
    setMigrationProgress(0);
    setMigrationFailed(false);

    try {
      const data = await request<{ failed?: number; sessions?: number; messages?: number; outcomes?: { status?: string; seed?: string; reason?: string }[] }>("config.database_migrate");
      const failed = Number(data.failed ?? 0);
      const failures = Array.isArray(data.outcomes)
        ? data.outcomes
          .filter((outcome: { status?: string }) => outcome.status === "failed")
          .map((outcome: { seed?: string; reason?: string }) => `${outcome.seed ?? "unknown"}: ${outcome.reason ?? "unknown error"}`)
        : [];
      setMigrationResult(failed === 0
        ? t().settings.migrateDone
          .replace("{sessions}", String(data.sessions ?? 0))
          .replace("{messages}", String(data.messages ?? 0))
        : t().settings.migratePartial
          .replace("{sessions}", String(data.sessions ?? 0))
          .replace("{failed}", String(failed))
          .replace("{reasons}", failures.join("\n")));
      setMigrationFailed(failed > 0);
      setMigrationPending(failed);
      setMigrationProgress(100);
      setMigrationPhase("done");
    } catch (e) {
      setMigrationResult(t().settings.migrateFailed.replace("{reason}", String(e)));
      setMigrationFailed(true);
      setMigrationPhase("done");
    }
    setDualWriteChecked(databaseEnabled());
  }

  async function finishMigration() {
    if (dualWriteChecked() !== databaseEnabled()) {
      setDatabaseEnabled(dualWriteChecked());
      dbToggled = true;
      await request("config.save", {
        apiKey: !apiKeyConfigured() || showApiKeyInput() ? apiKeyValue() : "",
        model: model(), baseUrl: baseUrl(),
        providerId: providerId(), endpoint: endpointId(),
        maxTokens: maxTokens(), contextLimit: contextLimit(),
        reasoningEffort: reasoningEffort(), autoCompactThreshold: autoCompactEnabled() ? autoCompactThreshold() : 0.0, lang: props.lang(),
        subagentModel: subModel(), subagentBaseUrl: subBaseUrl(),
        subagentApiKey: !subApiKeyConfigured() || showSubApiKeyInput() ? subApiKeyValue() : "",
        subagentMaxTokens: subMaxTokens(),
        subagentTimeoutSecs: subTimeout(), subagentDefaultTools: subTools(),
        databaseEnabled: dualWriteChecked(),
      });
      setSaved(true);
      setTimeout(() => setSaved(false), 2500);
    }
    setMigrationPhase("idle");
    setMigrationResult("");
  }

  // ── Category definitions ──
  const categories = (): Category[] => [
    { id: "models", label: t().settings.categoryModels },
    { id: "api", label: t().settings.categoryApi },
    { id: "context", label: t().settings.categoryContext },
    { id: "subagent", label: t().settings.categorySubagent },
    { id: "data", label: t().settings.categoryData },
    { id: "multimodal", label: t().settings.categoryMultimodal },
    { id: "appearance", label: t().settings.categoryAppearance },
    { id: "advanced", label: t().settings.categoryAdvanced },
  ];

  const Loading = () => <div class="settings-loading">{t().chat.thinking}</div>;
  const ErrorState = () => (
    <div class="settings-error">
      <p>{t().settings.loadError ?? "Failed to load settings"}</p>
      <p class="settings-error-detail">{loadError()}</p>
      <button class="settings-save-btn" onClick={() => { setLoadError(null); refetchConfig(); }}>
        {t().settings.retry ?? "Retry"}
      </button>
    </div>
  );

  return (
    <div class="settings-page">
      <div class="settings-page-header">
        <h1>{t().settings.title}</h1>
        <button class={{ "settings-save-btn": true, saved: saved() }} onClick={save}>
          {saved() ? "✓ " + (t().settings.saved ?? "Saved") : t().settings.save}
        </button>
      </div>

      <Show when={!configLoading()} fallback={<Loading />}>
        <Show when={!loadError()} fallback={<ErrorState />}>
          <div class="settings-layout">
            {/* ── Left Nav ── */}
            <nav class="settings-nav">
              <For each={categories()}>
                {(cat) => (
                  <button
                    class={`settings-nav-item${cat.id === activeCategory() ? " active" : ""}`}
                    onClick={() => setActiveCategory(cat.id)}
                  >
                    {cat.label}
                  </button>
                )}
              </For>
            </nav>

            {/* ── Right Content ── */}
            <div class="settings-body">
              {/* Category: Models & Providers */}
              <Show when={activeCategory() === "models"}>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.sectionProvider}</h2>
                  <div class="settings-row">
                    <label>{t().settings.provider}</label>
                    <select value={providerId()} onChange={(e) => handleProviderChange(e.currentTarget.value)}>
                      <For each={providers()}>{(p: Provider) => <option value={p.id}>{p.display}</option>}</For>
                    </select>
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.endpoint}</label>
                    <select value={endpointId()} onChange={(e) => handleEndpointChange(e.currentTarget.value)}>
                      <For each={currentEndpoints()}>{(ep: Endpoint) => <option value={ep.id}>{ep.display}</option>}</For>
                    </select>
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.baseUrl}</label>
                    <input value={baseUrl()} onInput={(e) => setBaseUrl(e.currentTarget.value)} placeholder="https://api.deepseek.com/v1" />
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.model}</label>
                    <div class="settings-input-group">
                      <input list="model-suggestions" value={model()} onInput={(e) => setModel(e.currentTarget.value)} placeholder="e.g. deepseek-chat" />
                      <datalist id="model-suggestions"><For each={currentModels()}>{(m: string) => <option value={m} />}</For></datalist>
                      <div class="settings-hint">{t().settings.modelHint}</div>
                    </div>
                  </div>
                </section>
              </Show>

              {/* Category: API & Credentials */}
              <Show when={activeCategory() === "api"}>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.sectionApi}</h2>
                  <div class="settings-row">
                    <label>{t().settings.apiKey}</label>
                    <SecretInput
                      configured={apiKeyConfigured()}
                      showInput={showApiKeyInput()}
                      value={apiKeyValue()}
                      onInput={(v) => setApiKeyValue(v)}
                      onReplace={() => { setShowApiKeyInput(true); setApiKeyValue(""); }}
                      onCancel={() => { setShowApiKeyInput(false); setApiKeyValue(""); }}
                      placeholder="sk-..."
                      configuredLabel={t().settings.apiKeyConfigured}
                      replaceLabel={t().settings.apiKeyReplace}
                      cancelLabel={t().settings.cancel}
                      hint={t().settings.apiKeyHint}
                    />
                  </div>
                </section>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.subagentApiKey}</h2>
                  <div class="settings-row">
                    <label>{t().settings.subagentApiKey}</label>
                    <SecretInput
                      configured={subApiKeyConfigured()}
                      showInput={showSubApiKeyInput()}
                      value={subApiKeyValue()}
                      onInput={(v) => setSubApiKeyValue(v)}
                      onReplace={() => { setShowSubApiKeyInput(true); setSubApiKeyValue(""); }}
                      onCancel={() => { setShowSubApiKeyInput(false); setSubApiKeyValue(""); }}
                      placeholder={t().settings.subagentInherit}
                      configuredLabel={t().settings.apiKeyConfigured}
                      replaceLabel={t().settings.apiKeyReplace}
                      cancelLabel={t().settings.cancel}
                      hint={t().settings.subagentApiKeyHint}
                    />
                  </div>
                </section>
              </Show>

              {/* Category: Context & Reasoning */}
              <Show when={activeCategory() === "context"}>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.sectionModel}</h2>
                  <div class="settings-row">
                    <label>{t().settings.maxTokens}</label>
                    <input type="number" value={maxTokens()} onInput={(e) => setMaxTokens(parseInt(e.currentTarget.value) || 16384)} step={1024} />
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.contextLimit}</label>
                    <input type="number" value={contextLimit()} onInput={(e) => setContextLimit(parseInt(e.currentTarget.value) || 1000000)} step={100000} />
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.reasoningEffort}</label>
                    <select value={reasoningEffort()} onChange={(e) => setReasoningEffort(e.currentTarget.value)}>
                      <option value="high">high</option>
                      <option value="max">max</option>
                    </select>
                  </div>
                  <div class="settings-row">
                    <label>Auto-compact</label>
                    <div class="settings-input-group">
                      <label class="settings-toggle">
                        <input type="checkbox" checked={autoCompactEnabled()} onChange={(e) => setAutoCompactEnabled(e.currentTarget.checked)} />
                        <span>Enable automatic context compaction</span>
                      </label>
                      <div class="settings-hint">
                        When disabled, compaction is only triggered manually via the Compact button.
                      </div>
                    </div>
                  </div>
                  <Show when={autoCompactEnabled()}>
                    <div class="settings-row">
                      <label>Auto-compact threshold</label>
                      <div class="settings-input-group">
                        <div class="settings-range-row">
                          <input type="range" min="0.3" max="0.95" step="0.05" value={autoCompactThreshold()} onInput={(e) => setAutoCompactThreshold(parseFloat(e.currentTarget.value))} />
                          <span class="settings-range-value">{(autoCompactThreshold() * 100).toFixed(0)}%</span>
                        </div>
                        <div class="settings-hint">
                          When context exceeds {contextLimit() ? (contextLimit() / 1000).toFixed(0) + "K" : "limit"} × {(autoCompactThreshold() * 100).toFixed(0)}% ≈ {contextLimit() ? ((contextLimit() * autoCompactThreshold()) / 1000).toFixed(0) + "K" : "..."} tokens, auto-compact summarises old turns before the next gate request.
                        </div>
                      </div>
                    </div>
                  </Show>
                </section>
              </Show>

              {/* Category: Subagent */}
              <Show when={activeCategory() === "subagent"}>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.sectionSubagent}</h2>
                  <p class="settings-section-desc">{t().settings.subagentDesc}</p>
                  <div class="settings-row">
                    <label>{t().settings.subagentModel}</label>
                    <div class="settings-input-group">
                      <input value={subModel()} onInput={(e) => setSubModel(e.currentTarget.value)} placeholder={t().settings.subagentInherit} />
                      <div class="settings-hint">{t().settings.subagentModelHint}</div>
                    </div>
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.subagentBaseUrl}</label>
                    <input value={subBaseUrl()} onInput={(e) => setSubBaseUrl(e.currentTarget.value)} placeholder={t().settings.subagentInherit} />
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.subagentMaxTokens}</label>
                    <input type="number" value={subMaxTokens()} onInput={(e) => setSubMaxTokens(parseInt(e.currentTarget.value) || 4096)} step={512} />
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.subagentTimeout}</label>
                    <input type="number" value={subTimeout()} onInput={(e) => setSubTimeout(parseInt(e.currentTarget.value) || 120)} step={30} />
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.subagentTools}</label>
                    <div class="settings-input-group">
                      <div class="settings-checkbox-grid">
                        <For each={allTools() ?? []}>
                          {(name) => (
                            <label class={`settings-checkbox-item ${subTools().includes(name) ? "checked" : ""}`}>
                              <input type="checkbox" checked={subTools().includes(name)} onChange={() => toggleTool(name)} />
                              <span>{name}</span>
                            </label>
                          )}
                        </For>
                      </div>
                      <div class="settings-hint">{t().settings.subagentToolsHint}</div>
                    </div>
                  </div>
                </section>
              </Show>

              {/* Category: Multimodal */}
              <Show when={activeCategory() === "multimodal"}>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.multimodal.title}</h2>
                  <p class="settings-section-desc">{t().settings.multimodal.desc}</p>
                  <div class="settings-row">
                    <label>{t().settings.multimodal.enable}</label>
                    <div class="settings-input-group">
                      <label class="settings-toggle">
                        <input type="checkbox" checked={mmEnabled()} onChange={(e) => setMmEnabled(e.currentTarget.checked)} />
                        <span class="settings-toggle-track" />
                      </label>
                      <div class="settings-hint">{t().settings.multimodal.enableHint}</div>
                    </div>
                  </div>
                  <Show when={mmEnabled()}>
                    <div class="settings-row">
                      <label>{t().settings.multimodal.providerType}</label>
                      <select value={mmProviderType()} onChange={(e) => setMmProviderType(e.currentTarget.value)}>
                        <option value="mimo">MiMo (小米) — 云端 API</option>
                        <option value="ollama">Ollama — 本地模型</option>
                        <option value="lmstudio">LM Studio — 本地 (原生 API)</option>
                        <option value="openai_compat">OpenAI 兼容 — LM Studio / vLLM / 通用</option>
                      </select>
                      <div class="settings-hint">{t().settings.multimodal.providerTypeHint}</div>
                    </div>
                    <div class="settings-row">
                      <label>{t().settings.multimodal.apiKey}</label>
                      <SecretInput
                        configured={mmApiKeyConfigured()}
                        showInput={showMmApiKeyInput()}
                        value={mmApiKeyValue()}
                        onInput={setMmApiKeyValue}
                        onReplace={() => { setShowMmApiKeyInput(true); setMmApiKeyValue("") }}
                        onCancel={() => { setShowMmApiKeyInput(false); setMmApiKeyValue("") }}
                        placeholder={t().settings.multimodal.apiKeyPlaceholder}
                        configuredLabel={t().settings.multimodal.apiKeyConfigured}
                        replaceLabel={t().settings.apiKeyReplace}
                        cancelLabel={t().settings.cancel}
                        hint={t().settings.multimodal.apiKeyHint}
                      />
                    </div>
                    <div class="settings-row">
                      <label>{t().settings.multimodal.baseUrl}</label>
                      <input value={mmBaseUrl()} onInput={(e) => setMmBaseUrl(e.currentTarget.value)} placeholder={t().settings.multimodal.baseUrlPlaceholder} />
                    </div>
                    <div class="settings-row">
                      <label>{t().settings.multimodal.model}</label>
                      <input value={mmModel()} onInput={(e) => setMmModel(e.currentTarget.value)} />
                    </div>
                    <div class="settings-row">
                      <label>{t().settings.multimodal.maxTokens}</label>
                      <input type="number" value={mmMaxTokens()} onInput={(e) => setMmMaxTokens(parseInt(e.currentTarget.value) || 4096)} step={512} />
                    </div>
                  </Show>
                </section>
              </Show>

              {/* Category: Data & Storage */}
              <Show when={activeCategory() === "data"}>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.sectionDatabase}</h2>
                  <div class="settings-row">
                    <label>{t().settings.databaseEnabled}</label>
                    <div class="settings-input-group">
                      <label class="settings-toggle">
                        <input type="checkbox" checked={databaseEnabled()} onChange={(e) => void toggleDatabase(e.currentTarget.checked)} />
                        <span class="settings-toggle-track" />
                      </label>
                      <div class="settings-hint">{t().settings.databaseEnabledHint}</div>
                    </div>
                  </div>
                  <Show when={databaseEnabled()}>
                    <div class="settings-row" style="grid-template-columns:1fr;">
                      <p class="settings-db-desc">{t().settings.databaseDesc}</p>
                    </div>
                    <div class="settings-row">
                      <label>
                        {migrationPending() > 0
                          ? t().settings.migrateCount.replace("{n}", String(migrationPending()))
                          : t().settings.migrateUpToDate}
                      </label>
                      <Show when={migrationPending() > 0}>
                        <div class="settings-input-group">
                          <button class="settings-save-btn" onClick={startMigrate}>
                            {t().settings.migrateBtn}
                          </button>
                        </div>
                      </Show>
                    </div>
                  </Show>
                  <Show when={migrationResult()}>
                    <div class="settings-row">
                      <div class="settings-hint" style={`color: ${migrationFailed() ? "var(--accent-red)" : "var(--accent-green)"}`}>{migrationResult()}</div>
                    </div>
                  </Show>
                </section>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.sectionCompliance}</h2>
                  <div class="settings-row">
                    <label>{t().settings.complianceEnabled}</label>
                    <div class="settings-input-group">
                      <label class="settings-toggle">
                        <input type="checkbox" checked={complianceEnabled()} onChange={(e) => setComplianceEnabled(e.currentTarget.checked)} />
                        <span class="settings-toggle-track" />
                      </label>
                      <div class="settings-hint">{t().settings.complianceEnabledHint}</div>
                    </div>
                  </div>
                </section>
              </Show>

              {/* Category: Appearance & Language */}
              <Show when={activeCategory() === "appearance"}>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.sectionInterface}</h2>
                  <div class="settings-row">
                    <label>{t().settings.theme}</label>
                    <select value={props.theme()} onChange={(e) => props.onThemeChange(e.currentTarget.value as ThemeMode)}>
                      <option value="system">{t().settings.themeSystem}</option>
                      <option value="light">{t().settings.themeLight}</option>
                      <option value="dark">{t().settings.themeDark}</option>
                      <option value="dark-gray">{t().settings.themeDarkGray}</option>
                    </select>
                  </div>
                  <div class="settings-row">
                    <label>{t().settings.language}</label>
                    <select value={props.lang()} onChange={(e) => props.onLangChange(e.currentTarget.value as Lang)}>
                      <option value="en">English</option>
                      <option value="zh">中文</option>
                    </select>
                  </div>
                </section>
              </Show>

              {/* Category: Advanced */}
              <Show when={activeCategory() === "advanced"}>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.permissionControl}</h2>
                  <div class="settings-row">
                    <label>{t().settings.defaultPermission}</label>
                    <div class="settings-input-group">
                      <PermissionLevelSelect level={props.permissionLevel} onChange={props.onPermissionLevelChange} />
                      <div class="settings-hint">{t().settings.defaultPermissionHint}</div>
                    </div>
                  </div>
                </section>
                <section class="settings-section">
                  <h2 class="settings-section-title">{t().settings.sectionPerformance}</h2>
                  <div class="settings-row">
                    <label>{t().settings.tokenizerPath}</label>
                    <div class="settings-input-group">
                      <input value={tokenizerPath()} onInput={(e) => setTokenizerPath(e.currentTarget.value)} placeholder="path/to/tokenizer.json" />
                      <button class="settings-save-btn" style="margin-top:4px;padding:4px 12px;font-size:12px;" onClick={browseTokenizer}>{t().settings.tokenizerBrowse}</button>
                      <div class="settings-hint">{t().settings.tokenizerPathHint}</div>
                    </div>
                  </div>
                </section>
              </Show>
            </div>
          </div>
        </Show>
      </Show>

      {/* ── Migration Modal ── */}
      <Show when={migrationPhase() !== "idle"}>
        <div class="modal-overlay" onClick={() => { if (migrationPhase() === "done") setMigrationPhase("idle"); }}>
          <div class="modal-card" onClick={(e) => e.stopPropagation()}>
            <Show when={migrationPhase() === "confirm"}>
              <h3 style="margin:0 0 12px;font-size:16px;font-weight:600;">{t().settings.migrateWarningTitle}</h3>
              <p style="margin:0 0 8px;font-size:13px;color:var(--text-secondary);white-space:pre-wrap;">{t().settings.migrateWarningBody}</p>
              <p style="margin:0 0 16px;font-size:13px;color:var(--text-secondary);white-space:pre-wrap;">
                {t().settings.migrateConfirmBody.replace("{n}", String(migrationPending()))}
              </p>
              <div style="display:flex;gap:8px;justify-content:flex-end;">
                <button class="settings-save-btn" style="background:var(--text-muted);" onClick={() => setMigrationPhase("idle")}>
                  {t().settings.cancel}
                </button>
                <button class="settings-save-btn" onClick={doMigrate}>
                  {t().settings.migrateBtn}
                </button>
              </div>
            </Show>
            <Show when={migrationPhase() === "running"}>
              <h3 style="margin:0 0 4px;font-size:16px;font-weight:600;">{t().settings.migratingTitle}</h3>
              <p style="margin:0 0 16px;font-size:13px;color:var(--text-secondary);">{t().settings.migratingHint}</p>
              <div style="width:100%;height:6px;background:var(--bg-tertiary);border-radius:3px;overflow:hidden;margin-bottom:8px;">
                <div style={`height:100%;width:${migrationProgress()}%;background:var(--accent);border-radius:3px;transition:width 0.3s ease;`} />
              </div>
              <p style="margin:0;font-size:12px;color:var(--text-muted);text-align:center;">{migrationProgress()}%</p>
            </Show>
            <Show when={migrationPhase() === "done"}>
              <h3 style="margin:0 0 12px;font-size:16px;font-weight:600;">{t().settings.migrateDoneTitle}</h3>
              <p style="margin:0 0 16px;font-size:13px;color:var(--text-secondary);white-space:pre-wrap;">{migrationResult()}</p>
              <div style="display:flex;justify-content:flex-end;">
                <button class="settings-save-btn" onClick={() => { setMigrationPhase("idle"); setMigrationResult(""); setMigrationFailed(false); }}>
                  {t().settings.ok}
                </button>
              </div>
            </Show>
          </div>
        </div>
      </Show>
    </div>
  );
}
