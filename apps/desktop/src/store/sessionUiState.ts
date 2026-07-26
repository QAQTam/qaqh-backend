import { createSignal, type Accessor } from "solid-js";

export interface SessionUiState {
  workspace: Accessor<string>;
  setWorkspace(value: string): void;
  submittingInteractionId: Accessor<string | null>;
  beginInteractionSubmit(id: string): boolean;
  finishInteractionSubmit(id: string): void;
}

export function createSessionUiState(): SessionUiState {
  const [workspace, setWorkspaceSignal] = createSignal("");
  const [submittingInteractionId, setSubmittingInteractionId] = createSignal<string | null>(null);
  let submittingId: string | null = null;
  return {
    workspace,
    setWorkspace: setWorkspaceSignal,
    submittingInteractionId,
    beginInteractionSubmit(id) {
      if (!id || submittingId !== null) return false;
      submittingId = id;
      setSubmittingInteractionId(id);
      return true;
    },
    finishInteractionSubmit(id) {
      if (submittingId === id) {
        submittingId = null;
        setSubmittingInteractionId(null);
      }
    },
  };
}
