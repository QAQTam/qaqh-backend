import type { JSX } from "@solidjs/web";
import { Portal } from "@solidjs/web";

export default function InteractionModal(props: {
  label: string;
  children: JSX.Element;
}) {
  return (
    <Portal>
      <div class="interaction-modal-backdrop" data-interaction-modal>
        <div
          class="interaction-modal-card"
          role="dialog"
          aria-modal="true"
          aria-label={props.label}
        >
          {props.children}
        </div>
      </div>
    </Portal>
  );
}
