# DeepX Desktop

Web renderer（SolidJS）for the DeepX Rust daemon. The UI is hosted by the
WinUI3 shell (`apps/winui`, Rust + WebView2) or browsable directly against
the daemon's `/debug/` endpoint.

## Architecture

```text
SolidJS renderer
      │ narrow window.deepx bridge
WinUI3 shell (deepx-winui, Rust)
      │ WebMessage ↔ deepx-client (Ringing V1 HTTP/SSE)
deepx-daemon
```

The renderer has no Node.js integration and never reads the daemon discovery
token. The shell owns daemon discovery, lease, native dialogs and opening
local paths. In a plain browser (daemon `/debug/`), a read-only bridge
(`src/runtime/browserBridge.ts`) provides Ringing SSE observation.

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
