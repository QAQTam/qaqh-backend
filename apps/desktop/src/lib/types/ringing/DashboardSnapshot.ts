// Native replaceable dashboard activity payload (Rust `DashboardSnapshot`).
export type DashboardSnapshot = {
  seed: string;
  documents: Array<{ tag: string; path: string; turns_since_read: number; is_stale: boolean }>;
  recent_edits: Array<string>;
  tasks: Array<{ id: string; subject: string; description: string; status: string }>;
  current_todo_id?: string;
};
