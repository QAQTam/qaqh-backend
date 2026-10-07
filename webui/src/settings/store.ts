/**
 * 设置页状态:daemon 全局配置的读写面。
 *
 * 模块级单例(与 `tabs/store.ts` 同形)——配置本来就是 daemon 全局态,不属于
 * 任何标签;宿主对 `config.*`/`profile.*` 不注入 seed(webui/src-tauri/src/commands.rs
 * 的 `service_requires_session` 豁免),所以零会话/引导失败时也能打开设置。
 *
 * 三条不可省的现实:
 *  1. 写 = Merge Patch,只发差异(见 `./patch`);整包写回会覆盖别的端的改动。
 *  2. 保存成功后**重新 load**:`config.save` 的返回值恒为 null(qaqh-runtime/src/service.rs:538),
 *     且落盘前有服务端归一化(u64→u32 饱和、空串清除、掩码守卫)——草稿不是事实。
 *  3. 掩码 `"****"` 是读模型对密钥的全部表达(qaqh-config/src/dto.rs:22),
 *     前端永不可能读到明文,也就不可能把它写回。
 */
import { createMemo, createSignal } from "solid-js";
import { transport } from "../lib/transport";
import { applyConfigTheme } from "../lib/theme";
import { toast } from "../ui/toast";
import type { ConfigDto } from "../api/qaqh/ConfigDto";
import {
  buildPatch,
  isPatchEmpty,
  needsBypassConfirm,
  tierOf,
  validatePatch,
  type ConfigPatchWire,
  type PermissionTier,
} from "./patch";

const [open, setOpen] = createSignal(false);
const [loading, setLoading] = createSignal(false);
const [busy, setBusy] = createSignal(false);
const [error, setError] = createSignal<string | null>(null);
const [note, setNote] = createSignal<string | null>(null);
/** 正在把档位降到 skip-permissions:等待打字确认。 */
const [requireBypass, setRequireBypass] = createSignal(false);
const [bypassAck, setBypassAck] = createSignal("");
/** 关闭时仍有未保存改动:等待「放弃/留下」二选一。 */
const [confirmDiscard, setConfirmDiscard] = createSignal(false);

/** 已保存事实(后端权威投影)。 */
const [baseline, setBaseline] = createSignal<ConfigDto | null>(null);
/**
 * 编辑草稿。整对象浅拷贝而不是 `createStore`:store 的根类型含 `null`(未加载),
 * 而 Solid 无法给 null 根建代理。每次改动只换根引用,输入框的 `value` getter
 * 会重算但字符串相同即不写 DOM——光标不受扰动,代价只是三十个 getter。
 */
const [draft, setDraft] = createSignal<ConfigDto | null>(null);

const messageOf = (cause: unknown): string =>
  cause instanceof Error ? cause.message : String(cause);

/** 读权威配置并以此重置草稿。 */
async function fetchConfig(): Promise<void> {
  setLoading(true);
  setError(null);
  try {
    const dto = await transport.rpc<ConfigDto>("config.load");
    setBaseline(dto);
    setDraft(structuredClone(dto));
    applyConfigTheme(dto.theme);
    setRequireBypass(false);
    setBypassAck("");
    setConfirmDiscard(false);
  } catch (cause) {
    // ConfigDto 读模型拒绝残缺载荷(缺字段即失败),所以这里的错误文本就是
    // 「daemon 与壳版本不匹配」的真相,原样显示比兜底成空表单有用。
    setBaseline(null);
    setDraft(null);
    setError(messageOf(cause));
  } finally {
    setLoading(false);
  }
}

export const patch = createMemo<ConfigPatchWire>(() => {
  const base = baseline();
  const current = draft();
  if (base == null || current == null) return {};
  return buildPatch(base, current);
});

export const dirty = createMemo(() => !isPatchEmpty(patch()));
export const changedCount = createMemo(() => Object.keys(patch()).length);
/** 档位选择器读的是草稿:切换即 dirty,保存才生效。 */
export const currentTier = (): PermissionTier | null => {
  const current = draft();
  return current == null ? null : tierOf(current.permissionLevel);
};

export async function openSettings(): Promise<void> {
  setOpen(true);
  setNote(null);
  // 每次打开都重取:配置可能被别的端(TUI/另一份 webui/手改 config.toml)写过,
  // 拿旧基线算差异会把别人的改动当成「我要写回」的内容。
  await fetchConfig();
}

export const closeSettings = (): void => {
  setOpen(false);
  setConfirmDiscard(false);
  setRequireBypass(false);
  setBypassAck("");
};

/** 关闭入口:有未保存改动时先问「放弃还是留下」。 */
export function requestClose(): void {
  if (dirty()) {
    setConfirmDiscard(true);
    return;
  }
  closeSettings();
}

export function toggleSettings(): void {
  if (open()) requestClose();
  else void openSettings();
}

export function discard(): void {
  const base = baseline();
  if (base != null) setDraft(structuredClone(base));
  // 放弃编辑 = 主题回到基线值(编辑中是即时预览)。
  applyConfigTheme(base?.theme);
  setConfirmDiscard(false);
  setRequireBypass(false);
  setBypassAck("");
  setError(null);
  setNote(null);
}

export async function save(): Promise<void> {
  if (busy() || loading()) return;
  if (baseline() == null || draft() == null) {
    await fetchConfig();
    return;
  }
  const next = patch();
  const invalid = validatePatch(next);
  if (invalid != null) {
    setError(invalid);
    return;
  }
  if (isPatchEmpty(next)) {
    setNote("没有需要保存的改动");
    return;
  }
  if (needsBypassConfirm(next) && bypassAck().trim() !== "3") {
    setError(null);
    setRequireBypass(true);
    return;
  }
  setBusy(true);
  setError(null);
  try {
    await transport.rpc("config.save", next as Record<string, unknown>);
    await fetchConfig();
    setNote(`已保存 ${Object.keys(next).length} 项改动`);
    toast(`配置已保存(${Object.keys(next).length} 项),已热载生效`, "ok", { dedupeKey: "config-saved" });
  } catch (cause) {
    setError(messageOf(cause));
    toast(`保存失败:${messageOf(cause)}`, "err", { dedupeKey: "config-save-error" });
  } finally {
    setBusy(false);
  }
}

async function runService(
  method: string,
  params: Record<string, unknown>,
  done: string,
): Promise<void> {
  if (busy() || loading()) return;
  // profile 操作会整段重写配置(apply 换一套 model/baseUrl/effort…),
  // 带着未保存草稿去点它 = 草稿被服务端事实覆盖。拒绝,而不是静默丢改动。
  if (dirty()) {
    setError("有未保存的改动:先保存或放弃,再操作 profile");
    return;
  }
  setBusy(true);
  setError(null);
  try {
    await transport.rpc(method, params);
    await fetchConfig();
    setNote(done);
    toast(done, "ok", { dedupeKey: `profile:${method}` });
  } catch (cause) {
    setError(messageOf(cause));
    toast(`${method} 失败:${messageOf(cause)}`, "err", { dedupeKey: `profile:${method}` });
  } finally {
    setBusy(false);
  }
}

export const applyProfile = (name: string): Promise<void> =>
  runService("profile.apply", { name }, `已切换到 profile「${name}」`);

export const saveProfileAs = (name: string): Promise<void> =>
  runService("profile.save_current", { name }, `已把当前配置存为 profile「${name}」`);

export const deleteProfile = (name: string): Promise<void> =>
  runService("profile.delete", { name }, `已删除 profile「${name}」`);

/** 草稿字段写入:视图只经这两个口子改配置。动过草稿就收掉上一次的成功提示,
 *  否则「已保存 N 项改动」会一直压在脚注上,把当前的「没有改动」挡住。 */
export function setField<K extends keyof ConfigDto>(key: K, value: ConfigDto[K]): void {
  setDraft((prev) => (prev == null ? prev : { ...prev, [key]: value }));
  // 主题即时预览:改下拉立刻生效,保存落盘、放弃回滚(discard)。
  if (key === "theme") applyConfigTheme(value as string | null);
  setNote(null);
}

export function setSubagent<K extends keyof ConfigDto["subagent"]>(
  key: K,
  value: ConfigDto["subagent"][K],
): void {
  setDraft((prev) => (prev == null ? prev : { ...prev, subagent: { ...prev.subagent, [key]: value } }));
  setNote(null);
}

export { open, loading, busy, error, note, requireBypass, bypassAck, confirmDiscard, baseline, draft };
export { setConfirmDiscard, fetchConfig as reload, setBypassAck };
