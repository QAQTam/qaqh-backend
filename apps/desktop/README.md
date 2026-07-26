# DeepX Desktop

Independent Electron + SolidJS frontend for the DeepX Rust daemon.

## Architecture

```text
SolidJS renderer
      │ narrow contextBridge API
Electron preload
      │ validated IPC
Electron main
      │ authenticated loopback WebSocket
deepx-daemon (D:\DeepX)
```

The renderer has no Node.js integration and never reads the daemon discovery token. Electron Main owns daemon discovery, detached startup, request correlation, heartbeats, reconnects, leases, native dialogs, and opening local paths.

## Development

Build the backend daemon first:

```powershell
cd D:\DeepX
cargo build -p deepx-daemon
```

Then run the desktop project:

```powershell
cd D:\deepx-desktop
pnpm install
pnpm dev
```

Validation:

```powershell
pnpm typecheck
pnpm test
pnpm build
```

## Windows package

The reproducible packaging path downloads the backend release pinned by
`deepx-backend.lock.json`, verifies its manifest commit and SHA-256, stages it as
an Electron extra resource, and produces an x64 NSIS installer:

```powershell
pnpm package:win
```

Artifacts are written to `release/`. For source integration, use
`just package-local <backend-path>` or set `DEEPX_BACKEND_ROOT`; local builds must
still match the locked product and protocol versions. Use `pnpm package:dir` for
an unpacked application backed by the published daemon, or
`pnpm package:dir:local` for the sibling `D:\DeepX` checkout.

Closing Desktop does not stop the daemon or running Agent workers.
