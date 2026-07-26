// @vitest-environment jsdom
import { render } from "@solidjs/web";
import { expect, it } from "vitest";
import AppShell from "./AppShell";

it("renders the focused shell without permanent engineering panels", () => {
  const host = document.createElement("div");
  const dispose = render(() => <AppShell sidebar={<aside data-task-sidebar />} workspace={<><section data-thread-workspace /><section data-composer-dock /></>} />, host);
  expect(host.querySelector("[data-task-sidebar]")).not.toBeNull();
  expect(host.querySelector("[data-thread-workspace]")).not.toBeNull();
  expect(host.querySelector("[data-composer-dock]")).not.toBeNull();
  expect(host.querySelector(".status-panel,.info-bar,.open-tabs")).toBeNull();
  dispose();
});

it("mounts exactly one authoritative sidebar and workspace", () => {
  const host = document.createElement("div");
  const dispose = render(() => <AppShell
    sidebar={<aside data-task-sidebar />}
    workspace={<section data-active-workspace />}
  />, host);
  expect(host.querySelectorAll("[data-task-sidebar]")).toHaveLength(1);
  expect(host.querySelectorAll("[data-thread-workspace]")).toHaveLength(1);
  expect(host.querySelector(".sidebar")).toBeNull();
  dispose();
});
