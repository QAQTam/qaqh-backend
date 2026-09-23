import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";

const sources = [...new Bun.Glob("src/**/*.{ts,tsx}").scanSync({ cwd: import.meta.dir + "/.." })];

test("model data rendering has no HTML or inline-style injection sinks", () => {
  for (const path of sources) {
    const source = readFileSync(`${import.meta.dir}/../${path}`, "utf8");
    expect(source).not.toContain("innerHTML");
    expect(source).not.toContain("insertAdjacentHTML");
    expect(source).not.toContain("document.write");
    expect(source).not.toContain("dangerouslySetInnerHTML");
    expect(/\bstyle\s*=/.test(source)).toBe(false);
  }
});
