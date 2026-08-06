import type { JSX } from "@solidjs/web";

interface InteractionDockProps {
  children: JSX.Element;
}

export default function InteractionDock(props: InteractionDockProps) {
  return (
    <div class="interaction-dock">
      {props.children}
    </div>
  );
}
