/**
 * Diff 渲染的浏览器验收夹具(开发工具,不进产品构建)。
 * 三档输入各一份:常规多 hunk(词级高亮)、大文件(>400 行默认折叠 →
 * 展开验虚拟开窗)、超限文件(>1200 行 → 纯文本降级 + head/tail 截断)。
 */
import { render } from "@solidjs/web";
import { For, type Component } from "solid-js";
import { DiffList } from "./diff/DiffView";
import "./styles/app.css";

function makeDiff(path: string, totalLineCount: number, seed: string): string {
  const lines: string[] = [`diff --git a/${path} b/${path}`, `--- a/${path}`, `+++ b/${path}`];
  let oldNo = 1;
  let newNo = 1;
  let emitted = 0;
  let hunkIndex = 0;
  while (emitted < totalLineCount) {
    const hunkSize = Math.min(80, totalLineCount - emitted);
    lines.push(`@@ -${oldNo},${hunkSize} +${newNo},${hunkSize} @@`);
    for (let i = 0; i < hunkSize; i += 1) {
      const index = emitted + i;
      if (index % 7 === 3) {
        lines.push(`-const legacy${seed}_${index} = compute(${index}, "${seed}");`);
        lines.push(`+const modern${seed}_${index} = compute(${index}, "${seed}", { cached: true }); // changed`);
      } else if (index % 11 === 5) {
        lines.push(`+    item.push({ id: ${index}, tag: "${seed}", note: "touched" });`);
      } else {
        lines.push(`  // ${seed} line ${index}: stable context for the diff fixture`);
      }
    }
    oldNo += hunkSize;
    newNo += hunkSize;
    emitted += hunkSize;
    hunkIndex += 1;
  }
  return lines.join("\n");
}

const SAMPLES: Array<{ title: string; diff: string }> = [
  {
    title: "常规多 hunk(词级高亮 + 上下文省略行)",
    diff: makeDiff("src/normal.ts", 160, "a"),
  },
  {
    title: "大文件(>400 行:默认折叠,展开走虚拟开窗)",
    diff: makeDiff("src/large.ts", 900, "b"),
  },
  {
    title: "超限文件(>1200 行:纯文本降级,head+tail 800)",
    diff: makeDiff("src/huge.ts", 2000, "c"),
  },
];

const App: Component = () => (
  <div style={{ padding: "16px", "max-width": "900px" }}>
    <h1 style={{ "font-size": "16px", "margin-bottom": "12px" }}>diff check fixture</h1>
    <For each={SAMPLES}>
      {(sample) => (
        <section style={{ "margin-bottom": "20px" }}>
          <h2 style={{ "font-size": "13px", color: "#6f6f78", "margin-bottom": "6px" }}>{sample.title}</h2>
          <DiffList diffText={sample.diff} />
        </section>
      )}
    </For>
  </div>
);

render(() => <App />, document.getElementById("root")!);
