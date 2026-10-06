/**
 * 设置浮层:daemon 全局配置的读写面(spec 里的「第二阶段·设置页」)。
 *
 * 只呈现 `ConfigDto` 里真实存在的字段,写路径只发差异 patch(见 `./patch`)。
 * 三条来自后端的硬约束在 UI 上有直接体现:
 *  - 密钥读回来永远是 `"****"`(掩码,qaqh-config/src/dto.rs:22),所以密钥框是
 *    「留空 = 不改」的新增值输入,不做双向绑定;后端没有删除密钥的接口,UI 也不假装能删。
 *  - `"" = 保持现值` 的字符串(model/baseUrl/reasoningEffort)
 *    清空不会生效(dto.rs:10-11),所以这些框清空即视作未改动。
 *  - MCP/LSP 只有读模型(ConfigDto:56-59 注明写模型另立),这里只读展示。
 */
import {
  createEffect,
  createMemo,
  createSignal,
  For,
  Show,
  type Component,
  type ParentComponent,
} from "solid-js";
import IconX from "~icons/lucide/x";
import IconRotate from "~icons/lucide/rotate-ccw";
import { STR } from "../lib/strings";
import {
  MASK,
  PERMISSION_TIERS,
  REASONING_EFFORTS,
  WIRE_PROTOCOLS,
  THEME_OPTIONS,
  formatToolList,
  needsBypassConfirm,
  parseToolList,
  toInt,
  toNumber,
} from "./patch";
import {
  applyProfile,
  baseline,
  busy,
  bypassAck,
  changedCount,
  confirmDiscard,
  currentTier,
  deleteProfile,
  dirty,
  discard,
  draft,
  error,
  loading,
  note,
  patch,
  reload,
  requestClose,
  requireBypass,
  save,
  saveProfileAs,
  setBypassAck,
  setConfirmDiscard,
  setField,
  setSubagent,
} from "./store";

const Field: ParentComponent<{ label: string; hint?: string; wide?: boolean }> = (props) => (
  <label class={`field${props.wide ? " wide" : ""}`}>
    <span class="field-label">{props.label}</span>
    {props.children}
    <Show when={props.hint != null && props.hint !== ""}>
      <span class="field-hint">{props.hint}</span>
    </Show>
  </label>
);

const TextInput: Component<{
  value: string;
  onInput: (value: string) => void;
  placeholder?: string;
  type?: "text" | "password";
}> = (props) => (
  <input
    type={props.type ?? "text"}
    class="field-input"
    value={props.value}
    placeholder={props.placeholder ?? ""}
    onInput={(event) => props.onInput(event.currentTarget.value)}
  />
);

const Section: ParentComponent<{ title: string; desc?: string }> = (props) => (
  <section class="settings-section">
    <h3>{props.title}</h3>
    <Show when={props.desc != null && props.desc !== ""}>
      <p class="settings-section-desc">{props.desc}</p>
    </Show>
    <div class="settings-fields">{props.children}</div>
  </section>
);

const Switch: Component<{ label: string; checked: boolean; onToggle: (value: boolean) => void }> = (props) => (
  <label class="field switch">
    <input type="checkbox" checked={props.checked} onInput={(event) => props.onToggle(event.currentTarget.checked)} />
    <span>{props.label}</span>
  </label>
);

export const SettingsView: Component = () => {
  // 密钥与工具清单是「只进不出」的本地缓冲:它们的 DOM 文本不等于草稿值
  // (掩码不能回填,逗号串要解析成数组),回填会让用户和输入框互相打架。
  const [newKey, setNewKey] = createSignal("");
  const [subKey, setSubKey] = createSignal("");
  const [toolsInput, setToolsInput] = createSignal("");
  const [profileName, setProfileName] = createSignal("");

  // 基线换了(载入/保存/切 profile)才重置缓冲——它们不参与 patch 构建。
  createEffect(
    () => baseline(),
    (base) => {
      setNewKey("");
      setSubKey("");
      setToolsInput(base == null ? "" : formatToolList(base.subagent.defaultTools));
    },
  );

  const secretPlaceholder = (masked: string): string =>
    masked === MASK ? "已配置（留空保持不变）" : "未配置";

  /** 当前值可能不在后端允许词表里(旧配置留下的),补进选项免得下拉替用户改了值。 */
  const effortOptions = createMemo(() => {
    const current = draft()?.reasoningEffort ?? "";
    const list = [...REASONING_EFFORTS] as string[];
    if (current !== "" && !list.includes(current)) list.unshift(current);
    return list;
  });

  const bypassPending = createMemo(() => requireBypass() && needsBypassConfirm(patch()));

  return (
    <div
      class="settings-overlay"
      onMouseDown={(event) => {
        if (event.target === event.currentTarget) requestClose();
      }}
    >
      <div class="settings-modal" role="dialog" aria-modal="true" aria-label={STR.settings}>
        <header class="settings-head">
          <h2>{STR.settings}</h2>
          <Show when={(draft()?.activeProfile ?? "") !== ""}>
            <span class="settings-sub">profile · {draft()?.activeProfile}</span>
          </Show>
          <button type="button" class="settings-close" aria-label={STR.close} onClick={requestClose}>
            <IconX />
          </button>
        </header>

        <Show when={loading() && draft() == null}>
          <div class="settings-state">{STR.settingsLoading}</div>
        </Show>
        <Show when={draft() == null && !loading() && error() != null}>
          <div class="settings-state is-error">
            <span>{error()}</span>
            <button type="button" class="ghost-mini" onClick={() => void reload()}>
              {STR.retry}
            </button>
          </div>
        </Show>

        <Show when={draft()}>
          {(config) => (
            <>
              <div class="settings-body">
                <Section
                  title="端点与模型"
                  desc="BYOK 六个字段:endpoint / wire / apikey / model / maxTokens / contextLength。端点与模型清空不生效（后端语义：空串 = 保持现值）。"
                >
                  <Field label="endpoint" wide hint="scheme + host + 可选前缀;wire 的规范路径自动补">
                    <TextInput value={config().baseUrl} onInput={(value) => setField("baseUrl", value)} placeholder="https://…/v1" />
                  </Field>
                  <Field label="wire">
                    <select
                      class="field-input"
                      value={config().wire}
                      onChange={(event) => setField("wire", event.currentTarget.value)}
                    >
                      <For each={WIRE_PROTOCOLS}>{(wire) => <option value={wire}>{wire}</option>}</For>
                    </select>
                  </Field>
                  <Field label="model">
                    <TextInput value={config().model} onInput={(value) => setField("model", value)} placeholder="模型 id" />
                  </Field>
                  <Field label="API key" hint={STR.settingsNoDelete} wide>
                    <TextInput
                      type="password"
                      value={newKey()}
                      placeholder={secretPlaceholder(config().apiKey)}
                      onInput={(value) => {
                        setNewKey(value);
                        // 留空 → 草稿退回掩码,patch 里就没有 apiKey 这一项。
                        setField("apiKey", value.trim() === "" ? config().apiKey : value);
                      }}
                    />
                  </Field>
                  <Field label="reasoningEffort">
                    <select
                      class="field-input"
                      value={config().reasoningEffort}
                      onChange={(event) => setField("reasoningEffort", event.currentTarget.value)}
                    >
                      <For each={effortOptions()}>{(effort) => <option value={effort}>{effort}</option>}</For>
                    </select>
                  </Field>
                  <Field label="maxTokens">
                    <TextInput
                      value={String(config().maxTokens)}
                      onInput={(value) => {
                        const parsed = toInt(value);
                        if (parsed != null) setField("maxTokens", parsed);
                      }}
                    />
                  </Field>
                  <Field label="contextLength" hint="端点声明的窗口 = 本地压缩分母">
                    <TextInput
                      value={String(config().contextLength)}
                      onInput={(value) => {
                        const parsed = toInt(value);
                        if (parsed != null) setField("contextLength", parsed);
                      }}
                    />
                  </Field>
                </Section>

                <Section
                  title="权限档位"
                  desc="三档制(1..=3);改动随「保存」一起落盘,并由后端广播 reload 给活跃 worker。"
                >
                  <div class="tier-list">
                    <For each={PERMISSION_TIERS}>
                      {(item) => (
                        <label
                          class={`tier${config().permissionLevel === item.level ? " active" : ""}${
                            item.dangerous ? " dangerous" : ""
                          }`}
                        >
                          <input
                            type="radio"
                            name="permission-tier"
                            value={String(item.level)}
                            checked={config().permissionLevel === item.level}
                            onInput={() => setField("permissionLevel", item.level)}
                          />
                          <span class="tier-name">{item.name}</span>
                          <span class="tier-desc">{item.desc}</span>
                        </label>
                      )}
                    </For>
                  </div>
                  <Show when={currentTier() == null}>
                    <p class="field-hint">
                      读到的档位 {config().permissionLevel} 不在 1..=3 内——后端会拒绝保存,请先选一个合法档位。
                    </p>
                  </Show>
                  <Show when={bypassPending()}>
                    <div class="bypass-confirm" role="alert">
                      <span>{STR.settingsBypassWarn}</span>
                      <input
                        class="field-input bypass-ack"
                        value={bypassAck()}
                        placeholder="输入 3 确认"
                        onInput={(event) => setBypassAck(event.currentTarget.value)}
                      />
                    </div>
                  </Show>
                </Section>

                <Section title="上下文与压缩">
                  <Field label="autoCompactThreshold" hint="0 = 关闭自动压缩;合法区间 [0, 1]。">
                    <TextInput
                      value={String(config().autoCompactThreshold)}
                      onInput={(value) => {
                        const parsed = toNumber(value);
                        if (parsed != null) setField("autoCompactThreshold", parsed);
                      }}
                    />
                  </Field>
                  <Field label="tokenizerPath" hint="空 = 未设置(内置分词)。" wide>
                    <TextInput
                      value={config().tokenizerPath ?? ""}
                      onInput={(value) => setField("tokenizerPath", value)}
                      placeholder="…/tokenizer.json"
                    />
                  </Field>
                  <Switch
                    label="complianceEnabled"
                    checked={config().complianceEnabled}
                    onToggle={(value) => setField("complianceEnabled", value)}
                  />
                </Section>

                <Section title="外观" desc={STR.settingsThemeNote}>
                  <Field label="theme">
                    <select
                      class="field-input"
                      value={config().theme ?? ""}
                      onChange={(event) => setField("theme", event.currentTarget.value)}
                    >
                      <For each={THEME_OPTIONS}>{(item) => <option value={item.value}>{item.label}</option>}</For>
                    </select>
                  </Field>
                  <Field label="lang" hint="空 = 跟随系统;后端不校验取值。">
                    <TextInput value={config().lang ?? ""} onInput={(value) => setField("lang", value)} placeholder="zh" />
                  </Field>
                  <Field label="fontFamily" hint="空 = 系统默认字体。" wide>
                    <TextInput value={config().fontFamily} onInput={(value) => setField("fontFamily", value)} />
                  </Field>
                  <Switch
                    label="notificationsEnabled"
                    checked={config().notificationsEnabled}
                    onToggle={(value) => setField("notificationsEnabled", value)}
                  />
                </Section>

                <Section
                  title="子代理"
                  desc="留空 = 保持现值;数值字段受后端值域校验(maxTokens/timeoutSecs > 0,maxDepth 1..=16)。"
                >
                  <Field label="model">
                    <TextInput value={config().subagent.model} onInput={(value) => setSubagent("model", value)} />
                  </Field>
                  <Field label="baseUrl">
                    <TextInput value={config().subagent.baseUrl} onInput={(value) => setSubagent("baseUrl", value)} />
                  </Field>
                  <Field label="apiKey" hint={`当前:${config().subagent.apiKeySet ? "已配置" : "未配置"}`}>
                    <TextInput
                      type="password"
                      value={subKey()}
                      placeholder={secretPlaceholder(config().subagent.apiKey)}
                      onInput={(value) => {
                        setSubKey(value);
                        setSubagent("apiKey", value.trim() === "" ? config().subagent.apiKey : value);
                      }}
                    />
                  </Field>
                  <Field label="maxTokens">
                    <TextInput
                      value={String(config().subagent.maxTokens)}
                      onInput={(value) => {
                        const parsed = toInt(value);
                        if (parsed != null) setSubagent("maxTokens", parsed);
                      }}
                    />
                  </Field>
                  <Field label="timeoutSecs">
                    <TextInput
                      value={String(config().subagent.timeoutSecs)}
                      onInput={(value) => {
                        const parsed = toInt(value);
                        if (parsed != null) setSubagent("timeoutSecs", parsed);
                      }}
                    />
                  </Field>
                  <Field label="maxDepth">
                    <TextInput
                      value={String(config().subagent.maxDepth)}
                      onInput={(value) => {
                        const parsed = toInt(value);
                        if (parsed != null) setSubagent("maxDepth", parsed);
                      }}
                    />
                  </Field>
                  <Field label="messageInFlightPerPair" hint="0 = 不限">
                    <TextInput
                      value={String(config().subagent.messageInFlightPerPair)}
                      onInput={(value) => {
                        const parsed = toInt(value);
                        if (parsed != null) setSubagent("messageInFlightPerPair", parsed);
                      }}
                    />
                  </Field>
                  <Field label="messageOutboundPerSender" hint="0 = 不限">
                    <TextInput
                      value={String(config().subagent.messageOutboundPerSender)}
                      onInput={(value) => {
                        const parsed = toInt(value);
                        if (parsed != null) setSubagent("messageOutboundPerSender", parsed);
                      }}
                    />
                  </Field>
                  <Field label="defaultTools" hint="逗号分隔;留空 = 全部工具可用。" wide>
                    <TextInput
                      value={toolsInput()}
                      placeholder="read, write"
                      onInput={(value) => {
                        setToolsInput(value);
                        setSubagent("defaultTools", parseToolList(value));
                      }}
                    />
                  </Field>
                </Section>

                <Section title="profile" desc="切换会整段重写配置,所以有未保存改动时直接拒绝,不静默覆盖草稿。">
                  <div class="profile-list">
                    <For each={config().profiles}>
                      {(name) => (
                        <div class={`profile-row${name === config().activeProfile ? " active" : ""}`}>
                          <span class="profile-name">{name}</span>
                          <button type="button" class="ghost-mini" disabled={busy()} onClick={() => void applyProfile(name)}>
                            切换
                          </button>
                          <button
                            type="button"
                            class="ghost-mini is-danger"
                            disabled={busy() || name === config().activeProfile}
                            onClick={() => void deleteProfile(name)}
                          >
                            删除
                          </button>
                        </div>
                      )}
                    </For>
                  </div>
                  <Field label="把当前配置存为" wide>
                    <div class="profile-create">
                      <TextInput value={profileName()} onInput={setProfileName} placeholder="profile 名" />
                      <button
                        type="button"
                        class="ghost-mini"
                        disabled={busy() || profileName().trim() === ""}
                        onClick={() => {
                          const name = profileName().trim();
                          if (name === "") return;
                          setProfileName("");
                          void saveProfileAs(name);
                        }}
                      >
                        保存
                      </button>
                    </div>
                  </Field>
                </Section>

                <Section
                  title="MCP / LSP（只读）"
                  desc="后端只有读模型:写面随 workspace 隔离权限重构另立(ConfigDto:56-59)。"
                >
                  <div class="readonly-grid">
                    <span>mcp.enabled</span>
                    <b>{String(config().mcp.enabled)}</b>
                    <span>mcp.idleShutdownSecs</span>
                    <b>{String(config().mcp.idleShutdownSecs)}</b>
                    <span>lsp.enabled</span>
                    <b>{String(config().lsp.enabled)}</b>
                    <span>lsp.idleShutdownSecs</span>
                    <b>{String(config().lsp.idleShutdownSecs)}</b>
                  </div>
                  <Show when={config().mcp.servers.length > 0}>
                    <table class="readonly-table">
                      <thead>
                        <tr>
                          <th>server</th>
                          <th>transport</th>
                          <th>command / url</th>
                          <th>tools</th>
                        </tr>
                      </thead>
                      <tbody>
                        <For each={config().mcp.servers}>
                          {(server) => (
                            <tr>
                              <td>{server.name}</td>
                              <td>{server.transport}</td>
                              <td>{server.command !== "" ? server.command : server.url}</td>
                              <td>{server.tools == null ? "全部" : String(server.tools.length)}</td>
                            </tr>
                          )}
                        </For>
                      </tbody>
                    </table>
                  </Show>
                  <Show when={config().lsp.servers.length > 0}>
                    <table class="readonly-table">
                      <thead>
                        <tr>
                          <th>server</th>
                          <th>command</th>
                          <th>extensions</th>
                        </tr>
                      </thead>
                      <tbody>
                        <For each={config().lsp.servers}>
                          {(server) => (
                            <tr>
                              <td>{server.name}</td>
                              <td>{server.command}</td>
                              <td>{server.extensions.join(", ")}</td>
                            </tr>
                          )}
                        </For>
                      </tbody>
                    </table>
                  </Show>
                </Section>
              </div>

              <footer class="settings-foot">
                <div class="settings-foot-state">
                  <Show when={error() != null}>
                    <span class="settings-error">{error()}</span>
                  </Show>
                  <Show when={error() == null && note() != null}>
                    <span class="settings-note">{note()}</span>
                  </Show>
                  <Show when={error() == null && note() == null}>
                    <span class="settings-muted">
                      {changedCount() > 0 ? `${STR.settingsChanged} ${changedCount()} 项` : STR.settingsClean}
                    </span>
                  </Show>
                </div>
                <div class="settings-foot-actions">
                  <Show when={confirmDiscard()}>
                    <span class="settings-discard">
                      {STR.settingsDiscardAsk}
                      <button type="button" class="ghost-mini" onClick={discard}>
                        {STR.settingsDiscardYes}
                      </button>
                      <button type="button" class="ghost-mini" onClick={() => setConfirmDiscard(false)}>
                        {STR.settingsStay}
                      </button>
                    </span>
                  </Show>
                  <button type="button" class="ghost-mini" disabled={busy()} onClick={() => void reload()}>
                    <IconRotate />
                    {STR.settingsReload}
                  </button>
                  <button
                    type="button"
                    class="primary-mini"
                    disabled={busy() || loading() || !dirty()}
                    onClick={() => void save()}
                  >
                    {busy() ? STR.settingsSaving : STR.settingsSave}
                  </button>
                </div>
              </footer>
            </>
          )}
        </Show>
      </div>
    </div>
  );
};
