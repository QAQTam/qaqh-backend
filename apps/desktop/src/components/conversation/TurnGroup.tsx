import { createSignal, For, Match, Show, Switch } from "solid-js";
import type { ChangeReviewFile, RoundRenderEntry, TurnViewModel } from "../../presentation/turnProjection";
import ProcessTimeline from "../process/ProcessTimeline";
import AssistantAnswer from "./AssistantAnswer";
import UserPromptBubble from "./UserPromptBubble";
import { useI18n } from "../../i18n";

export type ProcessStatus = "running" | "waiting" | "completed" | "failed" | "cancelled";

/** Session-level preference: once the user expands a timeline, default all timelines to expanded. */
const [preferExpanded, setPreferExpanded] = createSignal(false);

type ProcessEntry = Extract<RoundRenderEntry, { kind: "process" }>;
type AssistantEntry = Extract<RoundRenderEntry, { kind: "assistant" }>;

export default function TurnGroup(props: { turn: TurnViewModel; onReviewChanges?: (changes: ChangeReviewFile[]) => void }) {
  const { t } = useI18n();
  const status = () => props.turn.status as ProcessStatus;
  const changes = () => props.turn.changes ?? [];
  const changeTotals = () => changes().reduce(
    (sum, change) => ({ added: sum.added + change.added, removed: sum.removed + change.removed }),
    { added: 0, removed: 0 },
  );

  const onExpand = () => setPreferExpanded(true);

  // Collect all process items across rounds (merged into one group) + assistant entries in order.
  const merged = () => {
    const output: RoundRenderEntry[] = [];
    for (const round of props.turn.rounds) {
      let pending: NonNullable<ProcessEntry["items"]> = [];
      for (const entry of round.entries) {
        if (entry.kind === "process") {
          pending.push(...entry.items);
        } else {
          if (pending.length > 0) {
            const hasTools = pending.some(i => i.kind === "tool");
            output.push({ kind: "process", id: `${round.roundNum}-proc`, items: pending.splice(0), hasTools });
          }
          output.push(entry);
        }
      }
      if (pending.length > 0) {
        const hasTools2 = pending.some(i => i.kind === "tool");
        output.push({ kind: "process", id: `${round.roundNum}-proc-end`, items: pending.splice(0), hasTools: hasTools2 });
      }
    }
    return output;
  };

  return (
    <article class="conversation-turn" data-turn={props.turn.turnId}>
      <UserPromptBubble text={props.turn.userPrompt} />

      <For each={merged()} keyed={true}>
        {(entry) => {
          const process = () => {
            const current = entry;
            return current.kind === "process" ? current : undefined;
          };
          const assistant = () => {
            const current = entry;
            return current.kind === "assistant" ? current : undefined;
          };
          return (
            <Switch>
              <Match when={process()}>
                {(current) => (
                  <div data-part="process">
                    <ProcessTimeline
                      items={current().items}
                      expandable={true}
                      defaultExpanded={preferExpanded()}
                      onExpand={onExpand}
                    />
                  </div>
                )}
              </Match>
              <Match when={assistant()}>
                {(current) => (
                  <AssistantAnswer
                    markdown={current().markdown}
                    streaming={current().streaming ?? false}
                  />
                )}
              </Match>
            </Switch>
          );
        }}
      </For>

      <Show when={props.turn.failure}>
        {failure => <div class="turn-failure" data-part="turn-failure" role="alert">
          <div class="turn-failure-heading">
            <strong>{t().chat.error}</strong>
            <code>{failure().code}</code>
          </div>
          <p>{failure().message}</p>
        </div>}
      </Show>

      <Show when={status() === "completed" && changes().length > 0}>
        <div class="turn-change-receipt" data-part="turn-change-receipt">
          <span class="turn-change-receipt-files">{t().review.changedFiles.replace("{n}", String(changes().length))}</span>
          <Show when={changeTotals().added > 0}><span class="turn-change-add">+{changeTotals().added}</span></Show>
          <Show when={changeTotals().removed > 0}><span class="turn-change-del">-{changeTotals().removed}</span></Show>
          <Show when={props.onReviewChanges}>
            <button type="button" class="turn-change-review" onClick={() => props.onReviewChanges?.(changes())}>{t().review.reviewChanges}</button>
          </Show>
        </div>
      </Show>
    </article>
  );
}
