// @vitest-environment jsdom

import { render } from "@solidjs/web";
import { describe, expect, it, vi } from "vitest";
import PermissionLevelSelect from "./PermissionLevelSelect";

describe("PermissionLevelSelect", () => {
  it("renders all four permission levels and reports changes", () => {
    const host = document.createElement("div");
    const onChange = vi.fn();
    const dispose = render(() => (
      <PermissionLevelSelect level={2} onChange={onChange} />
    ), host);

    const select = host.querySelector("select") as HTMLSelectElement;
    expect([...select.options].map((option) => option.value)).toEqual(["1", "2", "3", "4"]);
    expect([...select.options].map((option) => option.text)).toEqual([
      "L1 全部询问",
      "L2 读取免询问",
      "L3 工作区操作",
      "L4 完全访问",
    ]);
    select.value = "3";
    select.dispatchEvent(new Event("change", { bubbles: true }));
    expect(onChange).toHaveBeenCalledWith(3);
    dispose();
  });

  it("marks full access as dangerous", () => {
    const host = document.createElement("div");
    const dispose = render(() => (
      <PermissionLevelSelect level={4} onChange={vi.fn()} compact />
    ), host);

    expect(host.querySelector("[data-permission-level]")?.classList.contains("is-danger")).toBe(true);
    dispose();
  });
});
