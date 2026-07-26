import { createSignal, For, Show } from "solid-js";
import type { AskQuestion, AskAnswer } from "../../lib/types";

interface AskUserPromptProps {
  questions: AskQuestion[];
  onSubmit: (answers: AskAnswer[]) => Promise<void> | void;
  onDismiss: () => void;
}

export default function AskUserPrompt(props: AskUserPromptProps) {
  const [answers, setAnswers] = createSignal<Record<string, string>>({});
  const [customInputs, setCustomInputs] = createSignal<Record<string, string>>({});
  const [busy, setBusy] = createSignal(false);
  let submitting = false;

  const allAnswered = () =>
    props.questions.every((q) =>
      (customInputs()[q.id] || "").trim() || answers()[q.id],
    );

  function selectOption(qid: string, opt: string) {
    setAnswers((prev) => ({ ...prev, [qid]: opt }));
    // Clear custom input for this question when option is selected
    setCustomInputs((prev) => {
      const next = { ...prev };
      delete next[qid];
      return next;
    });
  }

  function handleCustomInput(qid: string, value: string) {
    setCustomInputs((prev) => ({ ...prev, [qid]: value }));
    if (value.trim()) {
      setAnswers((prev) => {
        const next = { ...prev };
        delete next[qid];
        return next;
      });
    }
  }

  async function handleSubmit() {
    if (!allAnswered() || submitting) return;
    submitting = true;
    setBusy(true);
    try {
      const result: AskAnswer[] = props.questions.map((q) => ({
        question_id: q.id,
        answer: (customInputs()[q.id] || "").trim() || answers()[q.id] || "",
      }));
      await props.onSubmit(result);
    } finally {
      submitting = false;
      setBusy(false);
    }
  }

  return (
    <section class="interaction-prompt ask-user-prompt">
      <div class="interaction-eyebrow">问题</div>

      <For each={props.questions}>
        {(q, i) => (
          <fieldset>
            <legend>
              {props.questions.length > 1 ? `${i() + 1}. ` : ""}
              {q.question}
            </legend>

            <Show when={q.options && q.options.length > 0}>
              <div class="interaction-options">
                <For each={q.options}>
                  {(opt) => (
                    <button
                      class={`interaction-option${answers()[q.id] === opt ? " selected" : ""}`}
                      onClick={() => selectOption(q.id, opt)}
                      disabled={busy()}
                    >
                      {opt}
                    </button>
                  )}
                </For>
              </div>
            </Show>

            <Show when={(!q.options?.length || q.allow_custom !== false)}>
              <input
                type="text"
                class="interaction-custom-input"
                placeholder="输入自定义答案..."
                value={customInputs()[q.id] || ""}
                onInput={(e) => handleCustomInput(q.id, e.currentTarget.value)}
                disabled={busy()}
              />
            </Show>
          </fieldset>
        )}
      </For>

      <div class="interaction-actions">
        <button
          class="interaction-reject"
          onClick={props.onDismiss}
          disabled={busy()}
        >
          跳过
        </button>
        <button
          class="interaction-submit"
          onClick={handleSubmit}
          disabled={!allAnswered() || busy()}
        >
          {busy() ? "..." : "确认"}
        </button>
      </div>
    </section>
  );
}
