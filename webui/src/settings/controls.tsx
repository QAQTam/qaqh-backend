/**
 * 设置页共用小控件(Field/TextInput/Section/Switch),从 SettingsView 拆出:
 * ProfileManager / PairingSection 与主视图共用同一套字段骨架。
 */
import { Show, type Component, type ParentComponent } from "solid-js";

export const Field: ParentComponent<{ label: string; hint?: string; wide?: boolean; error?: string }> = (props) => (
  <label class={`field${props.wide ? " wide" : ""}`}>
    <span class="field-label">{props.label}</span>
    {props.children}
    <Show when={props.error != null && props.error !== ""}>
      <span class="field-hint is-error">{props.error}</span>
    </Show>
    <Show when={(props.error == null || props.error === "") && props.hint != null && props.hint !== ""}>
      <span class="field-hint">{props.hint}</span>
    </Show>
  </label>
);

export const TextInput: Component<{
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

export const Section: ParentComponent<{ title: string; desc?: string; id?: string }> = (props) => (
  <section class="settings-section" id={props.id}>
    <h3>{props.title}</h3>
    <Show when={props.desc != null && props.desc !== ""}>
      <p class="settings-section-desc">{props.desc}</p>
    </Show>
    <div class="settings-fields">{props.children}</div>
  </section>
);

export const Switch: Component<{ label: string; checked: boolean; onToggle: (value: boolean) => void }> = (props) => (
  <label class="field switch">
    <input type="checkbox" checked={props.checked} onInput={(event) => props.onToggle(event.currentTarget.checked)} />
    <span>{props.label}</span>
  </label>
);
