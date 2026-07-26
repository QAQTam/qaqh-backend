import { createEffect, createMemo, createSignal, For, Show } from "solid-js";
import { useI18n } from "../i18n";
import type { SkillInfo, SkillRuntimeInfo } from "../lib/types";

interface SkillsViewProps {
  seed: string;
  available: SkillInfo[];
  active: string[];
  runtime?: SkillRuntimeInfo[];
  workspace?: string;
  catalogRevision?: string;
  contextEpoch?: number;
  tokenBudget?: number;
  tokenUsage?: number;
  diagnostics?: string[];
  onActivate: (name: string) => Promise<void>;
  onUnload: (name: string) => Promise<void>;
  onRetain?: (name: string) => Promise<void>;
  onReload: () => Promise<void>;
}

type ColumnState = "catalog" | "requested" | "active" | "review_due" | "unavailable";
type ViewSkill = SkillRuntimeInfo & { scope: string; path: string };

export default function SkillsView(props: SkillsViewProps) {
  const { t } = useI18n();
  const [search, setSearch] = createSignal("");
  const [expanded, setExpanded] = createSignal<Set<string>>(new Set());
  const [pending, setPending] = createSignal<Set<string>>(new Set());
  const [errors, setErrors] = createSignal<Record<string, string>>({});
  const [refreshing, setRefreshing] = createSignal(false);
  const [refreshError, setRefreshError] = createSignal<string | null>(null);
  const pendingTargets = new Map<string, boolean>();
  let refreshAwaitingCatalog = false;

  const metadata = createMemo(() => new Map(props.available.map(item => [item.name, item])));
  const items = createMemo<ViewSkill[]>(() => {
    const runtime = props.runtime?.length ? props.runtime : props.available.map(item => ({
      name: item.name,
      description: item.description,
      state: props.active.includes(item.name) ? "active" : "catalog",
      source: props.active.includes(item.name) ? "legacy" : "catalog",
      token_count: 0,
    } satisfies SkillRuntimeInfo));
    const q = search().trim().toLowerCase();
    return runtime
      .map(item => {
        const meta = metadata().get(item.name);
        return { ...item, scope: meta?.scope ?? "project", path: meta?.source ?? item.source };
      })
      .filter(item => !q || item.name.toLowerCase().includes(q) || item.description.toLowerCase().includes(q));
  });
  const columns = createMemo(() => {
    const value: Record<ColumnState, ViewSkill[]> = {
      catalog: [], requested: [], active: [], review_due: [], unavailable: [],
    };
    for (const item of items()) {
      const state = item.state as ColumnState;
      (value[state] ?? value.unavailable).push(item);
    }
    return value;
  });

  createEffect(
    () => ({ active: props.active, runtime: props.runtime, available: props.available, catalogRevision: props.catalogRevision }),
    ({ active, runtime }) => {
    setPending(current => {
      const next = new Set(current);
      for (const name of current) {
        const target = pendingTargets.get(name);
        const runtimeState = runtime?.find(item => item.name === name)?.state;
        const resolved = target
          ? (runtime?.length ? runtimeState === "requested" || runtimeState === "active" : active.includes(name))
          : (runtime?.length ? runtimeState === "catalog" || runtimeState === undefined : !active.includes(name));
        if (resolved) { next.delete(name); pendingTargets.delete(name); }
      }
      return next;
    });
    if (refreshAwaitingCatalog) {
      refreshAwaitingCatalog = false;
      setRefreshing(false);
    }
  });

  const labels: Record<ColumnState, string> = {
    catalog: t().skills.groupCatalog,
    requested: t().skills.groupRequested,
    active: t().skills.groupEnabled,
    review_due: t().skills.groupReviewDue,
    unavailable: t().skills.groupUnavailable,
  };

  function toggleExpand(name: string) {
    setExpanded(current => {
      const next = new Set(current);
      next.has(name) ? next.delete(name) : next.add(name);
      return next;
    });
  }

  async function operate(item: ViewSkill, action: "activate" | "release" | "retain") {
    if (!props.seed || pending().has(item.name)) return;
    setPending(current => new Set(current).add(item.name));
    pendingTargets.set(item.name, action === "activate" || action === "retain");
    setErrors(current => { const next = { ...current }; delete next[item.name]; return next; });
    try {
      if (action === "activate") await props.onActivate(item.name);
      else if (action === "retain") await (props.onRetain ?? props.onActivate)(item.name);
      else await props.onUnload(item.name);
    } catch (error) {
      setErrors(current => ({ ...current, [item.name]: String(error) }));
      pendingTargets.delete(item.name);
      setPending(current => { const next = new Set(current); next.delete(item.name); return next; });
    }
  }

  async function reload() {
    if (!props.seed || refreshing()) return;
    setRefreshing(true);
    refreshAwaitingCatalog = true;
    setRefreshError(null);
    try { await props.onReload(); }
    catch (error) { refreshAwaitingCatalog = false; setRefreshError(String(error)); setRefreshing(false); }
  }

  return <div class="skills-page">
    <div class="skills-header">
      <div>
        <h1>{t().skills.title}</h1>
        <div class="skill-runtime-summary">
          <span>{props.workspace || "workspace"}</span>
          <span>{t().skills.catalogMeta.replace("{revision}", props.catalogRevision?.slice(0, 8) || "-")}</span>
          <span>{t().skills.epochMeta.replace("{epoch}", String(props.contextEpoch ?? 0))}</span>
          <span>{t().skills.tokenMeta.replace("{used}", String(props.tokenUsage ?? 0)).replace("{budget}", String(props.tokenBudget ?? 0))}</span>
        </div>
      </div>
      <div class="skills-header-actions">
        <div class="skill-search">
          <input value={search()} onInput={event => setSearch(event.currentTarget.value)}
            placeholder={t().skills.searchPlaceholder} disabled={!props.seed} />
        </div>
        <button class="skill-refresh-btn" onClick={reload} disabled={!props.seed || refreshing()}>
          {refreshing() ? "…" : t().skills.refresh}
        </button>
      </div>
    </div>

    <Show when={refreshError()}><div class="skill-refresh-error" role="alert">{refreshError()}</div></Show>
    <Show when={props.diagnostics?.length}>
      <details class="skill-diagnostics"><summary>{t().skills.diagnostics.replace("{count}", String(props.diagnostics!.length))}</summary>
        <For each={props.diagnostics}>{item => <div>{item}</div>}</For>
      </details>
    </Show>

    <Show when={props.seed} fallback={<div class="skills-empty"><p>{t().skills.noSession}</p></div>}>
      <Show when={items().length} fallback={<div class="skills-empty"><p>{search() ? t().skills.noResults : t().skills.empty}</p></div>}>
        <div class="skills-body skills-workbench">
          <span hidden>{t().skills.groupProject}</span><span hidden>{t().skills.groupUser}</span>
          <For each={["catalog", "requested", "active", "review_due", "unavailable"] as ColumnState[]}>
            {state => <section class={`skill-group skill-column skill-column-${state}`}>
              <h2 class="skill-group-title">{labels[state]} <span>{columns()[state].length}</span></h2>
              <For each={columns()[state]}>
                {item => <SkillCard
                  item={item} expanded={expanded().has(item.name)} pending={pending().has(item.name)}
                  error={errors()[item.name]} disabled={!props.seed}
                  onExpand={() => toggleExpand(item.name)} onAction={action => operate(item, action)} />}
              </For>
            </section>}
          </For>
        </div>
      </Show>
    </Show>
  </div>;
}

function SkillCard(props: {
  item: ViewSkill;
  expanded: boolean;
  pending: boolean;
  error?: string;
  disabled: boolean;
  onExpand: () => void;
  onAction: (action: "activate" | "release" | "retain") => void;
}) {
  const { t } = useI18n();
  const action = () => {
    switch (props.item.state) {
      case "catalog": return <label class="skill-toggle"><input type="checkbox" checked={false} disabled={props.disabled}
        onChange={() => props.onAction("activate")} /><span class="skill-toggle-track" /></label>;
      case "requested": return <button disabled={props.disabled} onClick={() => props.onAction("release")}>{t().skills.cancel}</button>;
      case "active": return <label class="skill-toggle"><input type="checkbox" checked={true} disabled={props.disabled}
        onChange={() => props.onAction("release")} /><span class="skill-toggle-track" /></label>;
      case "review_due": return <><button disabled={props.disabled} onClick={() => props.onAction("retain")}>{t().skills.retain}</button><button disabled={props.disabled} onClick={() => props.onAction("release")}>{t().skills.unload}</button></>;
      default: return <button disabled={props.disabled} onClick={() => props.onAction("activate")}>{t().skills.retry}</button>;
    }
  };
  return <div class={`skill-row ${props.item.state}${props.pending ? " pending" : ""}`}>
    <div class="skill-row-main" onClick={props.onExpand}>
      <div class="skill-row-left"><span class="skill-name">{props.item.name}</span>
        <span class="skill-desc-excerpt">{props.item.description}</span></div>
      <div class="skill-row-meta"><span class="skill-scope-badge">{props.item.scope}</span>
        <Show when={props.item.lease_remaining !== undefined}><span>{t().skills.lease.replace("{seconds}", String(props.item.lease_remaining))}</span></Show>
        <span>{props.item.token_count}t</span></div>
    </div>
    <div class="skill-row-actions">{props.pending ? <span class="skill-spinner" /> : action()}</div>
    <Show when={props.expanded}><div class="skill-detail"><p>{props.item.path}</p><Show when={props.item.error}><p>{props.item.error}</p></Show></div></Show>
    <Show when={props.error}><div class="skill-error">{props.error}</div></Show>
  </div>;
}
