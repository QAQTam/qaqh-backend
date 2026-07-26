# DeepX Electron Desktop build system

set windows-shell := ["pwsh.exe", "-NoLogo", "-Command"]

default:
    @just --list

bootstrap:
    pnpm install --frozen-lockfile

dev backend="../DeepX":
    cargo build --manifest-path "{{backend}}/Cargo.toml" -p deepx-daemon
    $env:DEEPX_BACKEND_ROOT=(Resolve-Path "{{backend}}").Path; pnpm dev

check:
    pnpm typecheck

test:
    pnpm test

build:
    pnpm build

# Package with the exact daemon published by deepx-backend.lock.json.
package:
    pnpm package:win

# Package against a local backend checkout while enforcing version/protocol parity.
package-local backend="../DeepX":
    node scripts/prepare-daemon.mjs --backend-root "{{backend}}"
    pnpm build
    pnpm exec electron-builder --win nsis --x64 --publish never
