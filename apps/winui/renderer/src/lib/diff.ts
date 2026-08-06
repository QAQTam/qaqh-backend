// Shared diff rendering: unified diff → styled HTML.
// Used by ToolCallCard and GitDiffPanel.

export function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

export function isUnifiedDiff(text: string): boolean {
  return /^(--- (a\/|\/)|@@ -\d+)/m.test(text);
}

export function renderDiffHtml(text: string): string {
  const lines = text.split("\n");
  const rows: Array<{ line: string; oldLn: string; newLn: string; cls: string }> = [];
  let oldLn = 0, newLn = 0;
  let fileHdr = "";
  let summary = "";
  let started = false;

  for (const line of lines) {
    if (!started && !line.startsWith("--- ") && !line.startsWith("@@")) {
      if (line.trim()) summary += esc(line) + "\n";
      continue;
    }
    if (line.startsWith("--- ")) {
      fileHdr = esc(line.slice(4));
      started = true;
      continue;
    }
    if (line.startsWith("+++ ")) { continue; }
    if (!started) continue;

    const hunkMatch = line.match(/^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/);
    if (hunkMatch) {
      oldLn = parseInt(hunkMatch[1]) - 1;
      newLn = parseInt(hunkMatch[3]) - 1;
      rows.push({ line: esc(line), oldLn: "", newLn: "", cls: "diff-row-hunk" });
      continue;
    }

    if (line.startsWith("-")) {
      oldLn++;
      rows.push({ line: esc(line.slice(1)), oldLn: String(oldLn), newLn: "", cls: "diff-row-del" });
    } else if (line.startsWith("+")) {
      newLn++;
      rows.push({ line: esc(line.slice(1)), oldLn: "", newLn: String(newLn), cls: "diff-row-add" });
    } else {
      oldLn++; newLn++;
      rows.push({ line: esc(line), oldLn: String(oldLn), newLn: String(newLn), cls: "diff-row-ctx" });
    }
  }

  if (rows.length === 0) return "";

  let html = '<div class="diff-block">';
  if (summary) html += `<div class="diff-summary">${summary.trim()}</div>`;
  if (fileHdr) html += `<div class="diff-file-hdr">${fileHdr}</div>`;
  html += '<div class="diff-uni-wrap">';

  for (const row of rows) {
    html += `<div class="diff-uni-row ${row.cls}">`;
    html += `<span class="diff-uni-ln diff-uni-old">${row.oldLn}</span>`;
    html += `<span class="diff-uni-ln diff-uni-new">${row.newLn}</span>`;
    html += `<span class="diff-uni-body">${row.line}</span>`;
    html += '</div>';
  }

  html += '</div></div>';
  return html;
}
