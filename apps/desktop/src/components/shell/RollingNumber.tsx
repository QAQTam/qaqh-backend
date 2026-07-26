import { For, createEffect, createMemo, createSignal, untrack } from "solid-js";

type RollingFrame = {
  previous: string;
  current: string;
  direction: "up" | "down";
  revision: number;
};

type RollingCell = {
  id: string;
  previous: string;
  current: string;
  rolls: boolean;
  formatShift: boolean;
};

export function formatCompactNumber(value: number): string {
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(2)}M`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(1)}K`;
  return String(value);
}

export default function RollingNumber(props: {
  value: number;
  format?: (value: number) => string;
  ariaLabel?: string;
  class?: string;
}) {
  const initialValue = untrack(() => props.value);
  const initial = untrack(() => (props.format ?? formatCompactNumber)(initialValue));
  const [frame, setFrame] = createSignal<RollingFrame>({
    previous: initial,
    current: initial,
    direction: "up",
    revision: 0,
  });
  let previousValue = initialValue;

  createEffect(
    () => ({ value: props.value, format: props.format }),
    ({ value, format }) => {
      const next = (format ?? formatCompactNumber)(value);
      const direction = value >= previousValue ? "up" as const : "down" as const;
      previousValue = value;
      setFrame(current => current.current === next ? current : {
        previous: current.current,
        current: next,
        direction,
        revision: current.revision + 1,
      });
    },
  );

  const cells = createMemo<RollingCell[]>(() => {
    const current = frame().current;
    const previous = frame().previous;
    const canRoll = current.length === previous.length;
    return Array.from(current, (character, index) => {
      const oldCharacter = previous[index] ?? "";
      return {
        id: `${frame().revision}:${index}`,
        previous: oldCharacter,
        current: character,
        rolls: canRoll &&
          oldCharacter !== character &&
          /\d/.test(oldCharacter) &&
          /\d/.test(character),
        formatShift: !canRoll,
      };
    });
  });

  return (
    <span
      class={`rolling-number ${props.class ?? ""}`}
      aria-label={props.ariaLabel ?? frame().current}
      data-value={frame().current}
    >
      <span class="rolling-number-visual" aria-hidden="true">
        <For each={cells()}>
          {(cell) => (
            <span
              class={`rolling-number-cell ${cell.rolls ? `rolling-${frame().direction}` : ""} ${cell.formatShift ? "rolling-format-shift" : ""}`}
            >
              {cell.rolls && <span class="rolling-number-old">{cell.previous}</span>}
              <span class="rolling-number-current">{cell.current}</span>
            </span>
          )}
        </For>
      </span>
    </span>
  );
}
