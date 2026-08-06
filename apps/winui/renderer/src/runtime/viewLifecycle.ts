export interface ViewRuntimeResource {
  dispose(): void;
}

export function cleanupViewResources(
  runtimes: Iterable<ViewRuntimeResource>,
  listeners: Iterable<() => void>,
  unlistenTheme?: () => void,
): void {
  for (const runtime of runtimes) runtime.dispose();
  for (const unlisten of listeners) {
    try {
      unlisten();
    } catch {
      // Listener was already removed.
    }
  }
  unlistenTheme?.();
}
