/**
 * 标签栏(spec §5.1/§5.2):[状态点] 标题 [关闭],末尾「+」;
 * 溢出横向滚动(隐藏滚动条 + 两端渐隐),当前标签自动滚入视野;
 * 状态点静态不闪烁,aria-label 同 title。
 */
import { createEffect, For, Show, type Component } from "solid-js";
import IconPlus from "~icons/lucide/plus";
import IconX from "~icons/lucide/x";
import { STR } from "../lib/strings";
import type { SessionStore } from "../session/store";

const Dot: Component<{ store: SessionStore; active: boolean }> = (props) => {
  const kind = (): { cls: string; label: string } | null => {
    if (!props.active && props.store.hasNewReply[0]()) return { cls: "new", label: STR.hasNewReply };
    // 活动状态是后端事实(session.activity / ControlDelta::Activity);failedTurns 与
    // pending 是本地/RPC 侧的补充真相,同权参与判定。
    const activity = props.store.activity[0]();
    if (activity === "failed" || activity === "interrupted" || props.store.state[0].failedTurns > 0) {
      return { cls: "err", label: STR.error };
    }
    if (activity === "waiting_user" || props.store.pending[0]().length > 0) return { cls: "warn", label: STR.waitingYou };
    if (activity === "working") return { cls: "run", label: STR.running };
    if (activity === "disconnected") return { cls: "off", label: STR.disconnected };
    // idle = 后端明示「没有活动在跑」:回合已收尾。尚未收到任何活动信号时不画点。
    if (activity === "idle") return { cls: "done", label: STR.idle };
    return null;
  };
  return (
    <Show when={kind() != null}>
      <span class={`tab-dot ${kind()!.cls}`} role="img" aria-label={kind()!.label} title={kind()!.label} />
    </Show>
  );
};

export const TabBar: Component<{
  tabs: Array<{ id: string; seed: string; store: SessionStore }>;
  activeId: string | null;
  creating: boolean;
  canCreate: boolean;
  onSelect: (id: string) => void;
  onClose: (id: string) => void;
  onCreate: () => void;
}> = (props) => {
  let scroller: HTMLDivElement | undefined;
  createEffect(
    () => props.activeId,
    () => {
      scroller?.querySelector(".tab.active")?.scrollIntoView({ block: "nearest", inline: "nearest" });
    },
  );
  const titleOf = (store: SessionStore): string => store.title[0]() ?? STR.newTab;
  return (
    <header id="tabbar">
      <div class="tab-scroll" ref={scroller} role="tablist" aria-label="会话">
        <For each={props.tabs}>
          {(tab) => (
            <div
              role="tab"
              tabindex={-1}
              class={{ tab: true, active: tab.id === props.activeId }}
              aria-selected={tab.id === props.activeId ? "true" : "false"}
              title={titleOf(tab.store)}
              onClick={() => props.onSelect(tab.id)}
              onKeyDown={(e) => {
                if (e.key === "Enter" || e.key === " ") props.onSelect(tab.id);
              }}
            >
              <Dot store={tab.store} active={tab.id === props.activeId} />
              <span class="tab-title">{titleOf(tab.store)}</span>
              <button
                type="button"
                class="tab-close"
                aria-label="关闭标签"
                title="关闭标签"
                onClick={(e) => {
                  e.stopPropagation();
                  props.onClose(tab.id);
                }}
              >
                <IconX />
              </button>
            </div>
          )}
        </For>
        <button
          type="button"
          class="tab-add"
          aria-label={STR.newTab}
          title={props.canCreate ? STR.newTab : "需要至少一个活动会话才能新建"}
          disabled={props.creating || !props.canCreate}
          onClick={props.onCreate}
        >
          <IconPlus />
        </button>
      </div>
    </header>
  );
};
