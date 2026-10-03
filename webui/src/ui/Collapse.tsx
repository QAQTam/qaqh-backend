/** 展开收起容器(spec §13.2):grid-template-rows 0fr↔1fr + opacity。
 *  折叠时不挂载内容——一个会话的时间线/工具详情有数百 KB 文本与 diff,
 *  启动即全量挂载是首屏卡顿与常驻内存的主因;收起动画播完后再卸载。 */
import { createEffect, createSignal, onCleanup, Show, untrack, type ParentComponent } from "solid-js";

/** ≥ CSS --dur-out:保留收起过渡动画,播完再释放子树。 */
const UNMOUNT_DELAY_MS = 260;

export const Collapse: ParentComponent<{ open: boolean }> = (props) => {
  const [mounted, setMounted] = createSignal(untrack(() => props.open));
  let timer: ReturnType<typeof setTimeout> | null = null;
  createEffect(
    () => props.open,
    (open) => {
      if (timer != null) {
        clearTimeout(timer);
        timer = null;
      }
      if (open) {
        setMounted(true);
        return;
      }
      if (!untrack(mounted)) return;
      timer = setTimeout(() => {
        timer = null;
        setMounted(false);
      }, UNMOUNT_DELAY_MS);
    },
  );
  onCleanup(() => {
    if (timer != null) clearTimeout(timer);
  });
  return (
    <div class={`collapse${props.open ? " open" : ""}`} data-open={props.open}>
      <div class="collapse-inner">
        <Show when={mounted()}>{props.children}</Show>
      </div>
    </div>
  );
};
