/** Converts an agent-reported path into a compact, portable workspace display path. */
export function workspaceDisplayPath(path: string, workspace?: string): string {
  const normalized = path.replace(/\\/g, "/");
  const root = workspace?.replace(/\\/g, "/").replace(/\/+$/, "");
  if (root && normalized.toLowerCase().startsWith(`${root.toLowerCase()}/`)) {
    return normalized.slice(root.length + 1);
  }
  if (!root || normalized === root) return normalized;

  // Keep external paths recognisable without forcing the popover to overflow.
  const parts = normalized.split("/").filter(Boolean);
  return parts.length > 3 ? `…/${parts.slice(-3).join("/")}` : normalized;
}
