import { action, createEffect, createMemo, createSignal, Errored, For, isPending, Loading, onCleanup, onSettled, Show } from "solid-js";
import { request } from "../runtime/backendClient";
import { useI18n } from "../i18n";
import { renderDiffHtml } from "../lib/diff";

export interface GitFileEntry {
  path: string;
  change: "added" | "deleted" | "modified" | "renamed";
  lines_added: number;
  lines_removed: number;
  diffHtml?: string;
}

const CHANGE_COLORS: Record<string, string> = {
  added: "var(--green)",
  modified: "var(--yellow)",
  deleted: "var(--red)",
  renamed: "var(--purple)",
};

const CHANGE_ICONS: Record<string, string> = {
  added: "+",
  modified: "~",
  deleted: "\u2212",
  renamed: "\u2192",
};

interface GitDiffPanelProps {
  open: boolean;
  seed: string;
  /** Changes whenever an agent tool reports a write; used to refresh Git promptly. */
  changeRevision?: number;
  /** Relative path selected from the environment summary. */
  initialFile?: string;
  onClose: () => void;
}

export default function GitDiffPanel(props: GitDiffPanelProps) {
  const { t } = useI18n();
  const [files, setFiles] = createSignal<GitFileEntry[]>([]);
  const [loading, setLoading] = createSignal(false);
  const [listError, setListError] = createSignal<string | null>(null);
  const [branch, setBranch] = createSignal("");
  const [branches, setBranches] = createSignal<{ name: string; current: boolean }[]>([]);
  const [switching, setSwitching] = createSignal(false);
  const [committing, setCommitting] = createSignal(false);
  const [commitMessage, setCommitMessage] = createSignal("");
  const [selectedFile, setSelectedFile] = createSignal<string | null>(null);
  const [showSwitchPrompt, setShowSwitchPrompt] = createSignal(false);
  const [pendingBranch, setPendingBranch] = createSignal("");
  const [actionError, setActionError] = createSignal<string | null>(null);
  const [actionNotice, setActionNotice] = createSignal<string | null>(null);
  let refreshing = false;
  let pollIntervalMs = 4_000;
  let pollHandle: ReturnType<typeof setTimeout> | undefined;

  function scheduleNextPoll(): void {
    pollHandle = setTimeout(async () => {
      if (!props.open || !props.seed) return;
      const ok = await tryRefresh();
      if (ok) {
        pollIntervalMs = 4_000;
      } else {
        pollIntervalMs = Math.min(pollIntervalMs * 2, 32_000);
        setTimeout(() => { pollIntervalMs = 4_000; }, 60_000);
      }
      if (props.open && props.seed) scheduleNextPoll();
    }, pollIntervalMs);
  }

  async function tryRefresh(): Promise<boolean> {
    if (!props.seed || refreshing) return true;
    refreshing = true;
    setLoading(true);
    setListError(null);
    try {
      const raw = await request<GitFileEntry[]>("git.diff", { seed: props.seed });
      setFiles(raw.map(file => ({ ...file, diffHtml: undefined })));
      const selected = selectedFile();
      if (selected && !raw.some(file => file.path === selected)) {
        setSelectedFile(null);
      }
      return true;
    } catch (e) {
      console.error("git_diff error:", e);
      setFiles([]);
      setListError(String(e));
      return false;
    } finally {
      setLoading(false);
      refreshing = false;
    }
  }

  let lastPolledRevision = 0;
  let pollDebounceTimer: number | undefined;

  createEffect(
    () => ({ open: props.open, seed: props.seed, revision: props.changeRevision }),
    ({ open, seed, revision }) => {
    if (!open || !seed) return;
    // Debounce: skip rapid-fire re-triggers from CodeDelta events.
    // Only poll if revision advanced and we aren't already scheduled.
    if (revision === lastPolledRevision) return;
    if (revision !== lastPolledRevision) {
      if (typeof revision === "number") lastPolledRevision = revision;
      if (pollDebounceTimer) clearTimeout(pollDebounceTimer);
      pollDebounceTimer = window.setTimeout(() => {
        pollDebounceTimer = undefined;
        void tryRefresh();
      }, revision ? 500 : 0);
    }
    if (pollHandle) { clearTimeout(pollHandle); pollHandle = undefined; }
    pollIntervalMs = 4_000;
    void loadBranches();
    scheduleNextPoll();
    onCleanup(() => {
      if (pollDebounceTimer) window.clearTimeout(pollDebounceTimer);
      if (pollHandle) clearTimeout(pollHandle);
    });
  });

  createEffect(
    () => ({ open: props.open, file: props.initialFile, files: files(), selected: selectedFile() }),
    ({ open, file, files, selected }) => {
    if (open && file && files.some(entry => entry.path === file) && selected !== file) {
      setSelectedFile(file);
    }
  });

  // ── Reset state when closed ──
  createEffect(
    () => !props.open,
    () => {
    if (!props.open) {
      setFiles([]);
      setListError(null);
      setBranch("");
      setBranches([]);
      setSelectedFile(null);
      setCommitMessage("");
      setShowSwitchPrompt(false);
    }
  });

  async function refreshFiles(): Promise<void> {
    tryRefresh().catch(e => console.error("manual refresh error:", e));
  }

  async function loadBranches() {
    try {
      const list = await request<{ name: string; current: boolean }[]>("git.branches", { seed: props.seed });
      setBranches(list);
      const current = list.find((b) => b.current)?.name ?? "";
      setBranch(current);
    } catch (_) {
      setBranches([]);
    }
  }

  // ── Async memo: diff content for selected file ──
  const diffHtml = createMemo(async () => {
    const path = selectedFile();
    if (!path) return null;

    const cached = files().find(f => f.path === path);
    if (cached?.diffHtml) return cached.diffHtml;

    const rawDiff = await request<string>("git.file_diff", {
      seed: props.seed,
      filePath: path,
    });
    const html = renderDiffHtml(rawDiff) ||
      '<div class="git-workspace-empty">No diff available</div>';
    setFiles(prev => prev.map(f => f.path === path ? { ...f, diffHtml: html } : f));
    return html;
  });

  async function switchBranch(name: string) {
    if (name === branch()) return;
    if (files().length > 0) {
      setPendingBranch(name);
      setShowSwitchPrompt(true);
      return;
    }
    await doSwitch(name);
  }

  const doSwitch = action(async function* (name: string, stash: boolean = false) {
    setSwitching(true);
    setShowSwitchPrompt(false);
    try {
      yield request("git.switch_branch", {
        seed: props.seed,
        branch: name,
        stash,
      });
      await refreshFiles();
      await loadBranches();
      setActionError(null);
      setActionNotice(`Switched to ${name}`);
    } catch (e) {
      console.error("switch branch:", e);
      setActionError(String(e));
      setActionNotice(null);
    } finally {
      setSwitching(false);
    }
  });

  const commit = action(async function* () {
    const msg = commitMessage().trim();
    if (!msg) return;
    setCommitting(true);
    try {
      yield request("git.commit", {
        seed: props.seed,
        message: msg,
      });
      setCommitMessage("");
      setSelectedFile(null);
      await refreshFiles();
      setActionError(null);
      setActionNotice("Commit created");
    } catch (e) {
      console.error("commit:", e);
      setActionError(String(e));
      setActionNotice(null);
    } finally {
      setCommitting(false);
    }
  });

  const totalStats = () => {
    let a = 0,
      r = 0;
    for (const f of files()) {
      a += f.lines_added;
      r += f.lines_removed;
    }
    return { added: a, removed: r };
  };

  // ── Don't render when closed ──
  if (!props.open) return null;

  return (
    <div class="git-workspace-overlay" onClick={props.onClose}>
      <div class="git-workspace" onClick={(e) => e.stopPropagation()}>
        {/* ── Header ── */}
        <div class="git-workspace-header">
          <span class="git-workspace-title">{t().status.gitChanges}</span>

          <Show when={branches().length > 0} fallback={
            <span class="git-workspace-branch-select" style="cursor:default;">
              {branch() || "—"}
            </span>
          }>
            <select
              class="git-workspace-branch-select"
              value={branch()}
              onChange={(e) => switchBranch(e.currentTarget.value)}
              disabled={switching()}
            >
              <For each={branches()}>
                {(b) => (
                  <option value={b.name} selected={b.current}>
                    {b.name}
                  </option>
                )}
              </For>
            </select>
          </Show>

          <Show when={switching()}>
            <span class="git-spinner">⟳</span>
          </Show>

          <div class="git-workspace-stats">
            <Show when={files().length > 0}>
              <span>{files().length} {t().status.files}</span>
            </Show>
            <Show when={totalStats().added > 0}>
              <span class="git-workspace-stat-add">+{totalStats().added}</span>
            </Show>
            <Show when={totalStats().removed > 0}>
              <span class="git-workspace-stat-del">-{totalStats().removed}</span>
            </Show>
          </div>

          <div class="git-workspace-actions">
            <button
              class="git-workspace-icon-btn active"
              title="Unified diff"
              aria-label="Unified diff"
            >
              U
            </button>
            <button
              class="git-workspace-icon-btn"
              title="Split diff unavailable"
              aria-label="Split diff"
              disabled
            >
              S
            </button>
            <button
              class="git-workspace-icon-btn"
              onClick={refreshFiles}
              disabled={loading()}
              title={t().skills.refresh}
            >
              {loading() ? <span class="git-spinner">⟳</span> : "↻"}
            </button>
            <button
              class="git-workspace-icon-btn"
              onClick={props.onClose}
              aria-label="Close"
            >
              ✕
            </button>
          </div>
        </div>

        {/* ── Branch switch prompt ── */}
        <Show when={showSwitchPrompt()}>
          <div class="git-switch-prompt">
            <span class="git-switch-prompt-msg">
              {t().status.switchPrompt.replace("{branch}", pendingBranch())}
            </span>
            <span class="git-switch-prompt-btns">
              <button class="git-switch-stash" onClick={() => doSwitch(pendingBranch(), true)}>
                {t().status.stashSwitch}
              </button>
              <button class="git-switch-cancel" onClick={() => { setShowSwitchPrompt(false); setPendingBranch(""); }}>
                {t().settings.cancel}
              </button>
            </span>
          </div>
        </Show>

        <Show when={actionError() || actionNotice()}>
          <div class={actionError() ? "git-action-feedback error" : "git-action-feedback"} role={actionError() ? "alert" : "status"}>
            {actionError() || actionNotice()}
          </div>
        </Show>

        {/* ── Body ── */}
        <Show
          when={files().length > 0}
          fallback={
            <Show when={listError()} fallback={
              <div class="git-workspace-empty">
                {loading() ? "Loading..." : t().status.noChanges}
              </div>
            }>
              <div class="git-workspace-error" role="alert">
                <span>{listError()}</span>
                <button class="git-workspace-icon-btn" onClick={refreshFiles}>Retry</button>
              </div>
            </Show>
          }
        >
          <div class="git-workspace-body">
            {/* Left: File list */}
            <div class="git-file-list">
              <For each={files()}>
                {(f) => (
                  <div
                    class={`git-file-item${selectedFile() === f.path ? " selected" : ""}`}
                    onClick={() => setSelectedFile(f.path)}
                  >
                    <span
                      class="git-file-change-icon"
                      style={`color: ${CHANGE_COLORS[f.change] || "var(--text-muted)"}`}
                    >
                      {CHANGE_ICONS[f.change] || "?"}
                    </span>
                    <span class="git-file-path">{f.path}</span>
                    <span class="git-file-stats">
                      <Show when={f.lines_added > 0}>
                        <span class="git-file-stat-add">+{f.lines_added}</span>
                      </Show>
                      <Show when={f.lines_removed > 0}>
                        <span class="git-file-stat-del">-{f.lines_removed}</span>
                      </Show>
                    </span>
                  </div>
                )}
              </For>
            </div>

            {/* Right: Diff view */}
            <div class="git-diff-view">
              <Show
                when={selectedFile()}
                fallback={
                  <div class="git-diff-view-empty">
                    {t().status.noFileSelected ?? "Select a file to view diff"}
                  </div>
                }
              >
                <Loading fallback={<div class="git-diff-view-loading">Loading diff...</div>}>
                  <Errored fallback={(err, reset) => (
                    <div class="git-diff-view-error">
                      <span>{String(err())}</span>
                      <button class="git-workspace-icon-btn" onClick={reset}>Retry</button>
                    </div>
                  )}>
                    <div class="git-diff-content" innerHTML={diffHtml() ?? ""} />
                  </Errored>
                </Loading>
              </Show>
            </div>
          </div>
        </Show>

        {/* ── Footer: Commit ── */}
        <Show when={files().length > 0}>
          <div class="git-workspace-footer">
            <input
              class="git-commit-input"
              type="text"
              value={commitMessage()}
              onInput={(e) => setCommitMessage(e.currentTarget.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter" && !e.shiftKey) {
                  e.preventDefault();
                  commit();
                }
              }}
              placeholder={t().status.commitPlaceholder}
              disabled={committing()}
            />
            <button
              class="git-commit-submit"
              onClick={commit}
              disabled={committing() || !commitMessage().trim()}
            >
              {committing() ? "..." : t().status.commit}
            </button>
          </div>
        </Show>
      </div>
    </div>
  );
}
