export type ProcessItem =
  | { kind: "reasoning"; id: string; content: string; elapsedMs?: number }
  | { kind: "assistant_progress"; id: string; markdown: string }
  | {
      kind: "tool";
      id: string;
      family: string;
      toolName: string;
      summary: string;
      argsJson?: string;
      output?: string;
      progress?: Array<{ stream: "stdout" | "stderr"; seq: number; chunk: string }>;
      success?: boolean;
    }
  | { kind: "group"; id: string; family: string; label: string; children: ProcessItem[] }
  | { kind: "interaction"; id: string; label: string; resolution: string }
  | { kind: "notice"; id: string; level: string; message: string };

function groupLabel(family: string, count: number): string {
  const verb = {
    read: "Viewed",
    write: "Changed",
    exec: "Ran",
    web: "Searched",
  }[family] ?? "Used";
  return `${verb} ${count} ${count === 1 ? "item" : "items"}`;
}

export function aggregateProcessItems(items: ProcessItem[]): ProcessItem[] {
  const result: ProcessItem[] = [];
  let run: Extract<ProcessItem, { kind: "tool" }>[] = [];

  const flush = () => {
    if (run.length >= 2) {
      result.push({
        kind: "group",
        id: `group-${run[0].family}-${run[0].id}`,
        family: run[0].family,
        label: groupLabel(run[0].family, run.length),
        children: run,
      });
    } else {
      result.push(...run);
    }
    run = [];
  };

  for (const item of items) {
    if (item.kind === "tool" && item.success === true) {
      if (run.length === 0 || run[0].family === item.family) {
        run.push(item);
      } else {
        flush();
        run.push(item);
      }
      continue;
    }
    flush();
    result.push(item);
  }
  flush();
  return result;
}
