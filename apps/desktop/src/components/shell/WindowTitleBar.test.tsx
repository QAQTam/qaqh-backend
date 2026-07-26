// @vitest-environment jsdom
import { render } from "@solidjs/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import WindowTitleBar from "./WindowTitleBar";

const windowApi = vi.hoisted(() => ({
  minimizeWindow: vi.fn(),
  toggleMaximizeWindow: vi.fn(async () => true),
  isWindowMaximized: vi.fn(async () => false),
  closeWindow: vi.fn(),
  onWindowMaximizedChanged: vi.fn((_listener: (maximized: boolean) => void) => () => {}),
}));

vi.mock("../../runtime/desktopApi", () => windowApi);

afterEach(() => {
  vi.clearAllMocks();
  document.body.replaceChildren();
});

describe("WindowTitleBar", () => {
  it("exposes custom minimize, maximize, and close controls", async () => {
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => <WindowTitleBar />, host);
    await vi.waitFor(() => expect(windowApi.isWindowMaximized).toHaveBeenCalledOnce());

    host.querySelector<HTMLButtonElement>("[data-window-minimize]")!.click();
    host.querySelector<HTMLButtonElement>("[data-window-maximize]")!.click();
    host.querySelector<HTMLButtonElement>("[data-window-close]")!.click();

    expect(windowApi.minimizeWindow).toHaveBeenCalledOnce();
    expect(windowApi.toggleMaximizeWindow).toHaveBeenCalledOnce();
    expect(windowApi.closeWindow).toHaveBeenCalledOnce();
    await vi.waitFor(() => {
      expect(host.querySelector("[data-window-maximize]")?.getAttribute("aria-label")).toBe("还原");
    });
    dispose();
  });

  it("tracks maximize changes reported by the main process", async () => {
    let publish: ((maximized: boolean) => void) | undefined;
    windowApi.onWindowMaximizedChanged.mockImplementationOnce(listener => {
      publish = listener;
      return () => {};
    });
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => <WindowTitleBar />, host);
    await vi.waitFor(() => expect(windowApi.onWindowMaximizedChanged).toHaveBeenCalledOnce());

    publish?.(true);

    await vi.waitFor(() => {
      expect(host.querySelector("[data-window-maximize]")?.getAttribute("aria-label")).toBe("还原");
    });
    dispose();
  });
});
