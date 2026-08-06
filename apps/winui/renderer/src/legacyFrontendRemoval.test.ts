import { expect, it } from "vitest";

const removed = [
  "store/chat.ts", "store/permissionQueue.ts", "store/environmentStore.ts",
  "store/orderedProgress.ts", "components/AskDialog.tsx", "components/AskForm.tsx",
  "components/ThinkingBlock.tsx", "components/ToolRow.tsx", "components/TokenChart.tsx",
  "components/StockChart.tsx", "components/SlashMenu.tsx", "components/ChangelogModal.tsx",
  "components/interactions/PlanApprovalPrompt.tsx",
  "components/DiffBody.tsx.1781723177.1782263662", "styles/sidebar.css",
  "styles/slash-menu.css", "styles/token-chart.css", "styles/changelog.css",
];

const modules = import.meta.glob("./**/*.{ts,tsx}", {
  eager: true,
  query: "?raw",
  import: "default",
}) as Record<string, string>;
const styles = import.meta.glob("./styles/*.css", {
  eager: true,
  query: "?raw",
  import: "default",
}) as Record<string, string>;

it("contains no legacy frontend implementation", () => {
  const existing = new Set([...Object.keys(modules), ...Object.keys(styles)]);
  expect(removed.filter(path => existing.has(`./${path}`))).toEqual([]);
  const source = Object.entries(modules)
    .filter(([path]) => !path.includes(".test."))
    .map(([, contents]) => contents)
    .join("\n");
  expect(source).not.toMatch(/createChatStore|chatStores|handleRoundDelta|handleToolCallPreview|handleRoundComplete|handleToolResults|handleExecProgress/);
  expect(source).not.toMatch(/\bpendingInteraction\b/);
  expect(source).not.toContain('<aside class="sidebar frost-panel">');
  expect(source).not.toMatch(/listen<Record<string, unknown>>|invoke<Record<string, unknown>\[\]>/);
});
