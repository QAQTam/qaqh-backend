/**
 * BYOK 档案管理器:六字段(端点/协议/密钥/模型/单回复上限/上下文窗口)+
 * 预设模板 + 命名档案切换。后端语义(config-api 契约):
 *  - `config.save` 的 Merge Patch 写的是**当前激活档案**,落盘即广播热载,
 *    切模型无需重启 daemon——所以「填字段 → 保存」就是完整的切换流程;
 *  - `profile.apply` 整段换激活档案(有未保存草稿时 store 会拒绝);
 *  - 密钥读回永远是掩码 `"****"`:留空 = 保持现值,填新值 = 覆盖。
 */
import { createMemo, createSignal, For, Show, type Component } from "solid-js";
import IconEye from "~icons/lucide/eye";
import IconEyeOff from "~icons/lucide/eye-off";
import { STR } from "../../lib/strings";
import { toast } from "../../ui/toast";
import { MASK, WIRE_PROTOCOLS, toInt } from "../patch";
import { applyProfile, busy, deleteProfile, draft, saveProfileAs, setField } from "../store";
import { Field, Section, TextInput } from "../controls";

/** 预设模板:预填 endpoint + wire(与后端 FIRST_RUN_CONFIG 的 DeepSeek 示例一致);
 *  model 各家变动频繁,留空让用户填,不硬编码会过期的 id。 */
const PRESET_TEMPLATES = [
  { id: "deepseek", label: "DeepSeek", baseUrl: "https://api.deepseek.com", wire: "openai" },
  { id: "openai", label: "OpenAI", baseUrl: "https://api.openai.com/v1", wire: "responses" },
  { id: "anthropic", label: "Anthropic", baseUrl: "https://api.anthropic.com", wire: "anthropic" },
  { id: "zhipu", label: "智谱 GLM", baseUrl: "https://open.bigmodel.cn/api/paas/v4", wire: "openai" },
  { id: "moonshot", label: "Kimi (Moonshot)", baseUrl: "https://api.moonshot.cn/v1", wire: "openai" },
  { id: "custom", label: "自定义", baseUrl: "", wire: "openai" },
] as const;

export const ProfileManager: Component = () => {
  const [showKey, setShowKey] = createSignal(false);
  const [newKey, setNewKey] = createSignal("");
  const [profileName, setProfileName] = createSignal("");

  const config = () => draft()!;
  const secretPlaceholder = (masked: string): string =>
    masked === MASK ? "已配置(留空保持不变)" : "未配置";

  /** 当前值可能不在后端允许词表里(旧配置留下的),补进选项免得下拉替用户改了值。 */
  const effortOptions = createMemo(() => {
    const current = config().reasoningEffort ?? "";
    const list = ["low", "medium", "high", "xhigh", "max"];
    if (current !== "" && !list.includes(current)) list.unshift(current);
    return list;
  });

  const applyTemplate = (id: string): void => {
    const template = PRESET_TEMPLATES.find((item) => item.id === id);
    if (template == null) return;
    if (template.baseUrl !== "") setField("baseUrl", template.baseUrl);
    setField("wire", template.wire);
    toast(`已套用模板「${template.label}」:填入 API key 与模型 id 后保存`, "info", { dedupeKey: "preset-applied" });
  };

  return (
    <>
      <Section
        id="section-byok"
        title="模型与端点(BYOK)"
        desc="填六个字段 → 保存,立即生效(热载,无需重启)。endpoint 与 model 清空不生效(后端语义:空串 = 保持现值)。"
      >
        <Field label="从模板新建" wide hint="只预填 endpoint 与 wire;API key 与模型 id 始终自己填。">
          <select
            class="field-input"
            value=""
            onChange={(event) => {
              applyTemplate(event.currentTarget.value);
              event.currentTarget.value = "";
            }}
          >
            <option value="" disabled>
              选择预设模板…
            </option>
            <For each={PRESET_TEMPLATES}>{(template) => <option value={template.id}>{template.label}</option>}</For>
          </select>
        </Field>
        <Field label="endpoint" wide hint="scheme + host + 可选前缀;wire 的规范路径自动补">
          <TextInput value={config().baseUrl} onInput={(value) => setField("baseUrl", value)} placeholder="https://…" />
        </Field>
        <Field label="wire">
          <select
            class="field-input"
            value={config().wire}
            onChange={(event) => setField("wire", event.currentTarget.value)}
          >
            <For each={[...WIRE_PROTOCOLS]}>{(wire) => <option value={wire}>{wire}</option>}</For>
          </select>
        </Field>
        <Field label="model">
          <TextInput value={config().model} onInput={(value) => setField("model", value)} placeholder="模型 id" />
        </Field>
        <Field label="API key" hint={STR.settingsNoDelete} wide>
          <div class="secret-input">
            <TextInput
              type={showKey() ? "text" : "password"}
              value={newKey()}
              placeholder={secretPlaceholder(config().apiKey)}
              onInput={(value) => {
                setNewKey(value);
                // 留空 → 草稿退回掩码,patch 里就没有 apiKey 这一项。
                setField("apiKey", value.trim() === "" ? config().apiKey : value);
              }}
            />
            <button
              type="button"
              class="icon-btn secret-eye"
              aria-label={showKey() ? "隐藏密钥" : "显示密钥"}
              onClick={() => setShowKey(!showKey())}
            >
              <Show when={showKey()} fallback={<IconEye />}>
                <IconEyeOff />
              </Show>
            </button>
          </div>
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
        <Field
          label="maxTokens"
          error={config().maxTokens <= 0 ? "必须是大于 0 的整数" : undefined}
        >
          <TextInput
            value={String(config().maxTokens)}
            onInput={(value) => {
              const parsed = toInt(value);
              if (parsed != null) setField("maxTokens", parsed);
            }}
          />
        </Field>
        <Field
          label="contextLength"
          hint="端点声明的窗口 = 本地压缩分母"
          error={config().contextLength <= 0 ? "必须是大于 0 的整数" : undefined}
        >
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
        id="section-profiles"
        title="档案预设"
        desc="把当前的六字段配置存成命名档案,随时一键切换(切换会整段覆盖草稿,有未保存改动时先保存)。"
      >
        <div class="profile-list">
          <For each={config().profiles}>
            {(name) => (
              <div class={`profile-row${name === config().activeProfile ? " active" : ""}`}>
                <span class="profile-name">{name}</span>
                <Show when={name === config().activeProfile}>
                  <span class="profile-active-tag">使用中</span>
                </Show>
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
            <TextInput value={profileName()} onInput={setProfileName} placeholder="档案名" />
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
    </>
  );
};
