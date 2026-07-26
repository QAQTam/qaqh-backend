# DeepX Desktop migration record

`deepx-desktop` is the sole graphical client for the DeepX daemon architecture.
The temporary Tauri parity implementation was retired from the backend workspace
after the Electron client reached feature parity.

The completed migration covers:

- daemon discovery, authenticated launch, reconnect cursor and session leases;
- session list/history, streaming output, tools, permissions, Ask, Plan, Skills,
  Goal/Task, Git, settings, statistics, cancel, compact and message rollback;
- native folder selection, confirmation dialogs, external links and local paths;
- explicit lease release during application shutdown;
- Windows packaging with `deepx-daemon.exe` detached from the desktop lifetime;
- a renderer boundary with sandboxing, context isolation and no Node.js access.

Backend storage and protocol compatibility remain owned by `deepx-daemon`.
The renderer must not read `.deepx`, the discovery token or business files directly.
Future desktop features belong in this repository and must use the versioned control
protocol instead of recreating backend logic in Electron.
