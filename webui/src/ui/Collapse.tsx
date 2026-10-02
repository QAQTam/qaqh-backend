/** 展开收起容器(spec §13.2):grid-template-rows 0fr↔1fr + opacity。 */
import type { ParentComponent } from "solid-js";

export const Collapse: ParentComponent<{ open: boolean }> = (props) => (
  <div class={`collapse${props.open ? " open" : ""}`} data-open={props.open}>
    <div class="collapse-inner">{props.children}</div>
  </div>
);
