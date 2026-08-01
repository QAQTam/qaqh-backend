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

    const options = [...host.querySelectorAll<HTMLButtonElement>("[data-permission-option]")];
    expect(options.map((option) => option.dataset.permissionOption)).toEqual(["1", "2", "3", "4"]);
    expect(options.map((option) => option.textContent?.trim())).toEqual([
      "L1全部询问",
      "L2读取免问",
      "L3工作区",
      "L4完全访问",
    ]);
    options[2].click();
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
