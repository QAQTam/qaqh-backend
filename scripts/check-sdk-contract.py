#!/usr/bin/env python3
"""Reproducible backend SDK artifact verification without a sibling SDK checkout.

Never installs tools or reads daemon credentials. --typescript additionally requires
an already-installed tsc and checks an independent strict TypeScript consumer.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent


def run(*args: str, env: dict[str, str] | None = None) -> None:
    subprocess.run(args, cwd=ROOT, env=env, check=True)


def verify(directory: Path) -> dict:
    manifest = json.loads((directory / "manifest.json").read_text())
    files = manifest["files"]
    actual = sorted(
        p.relative_to(directory).as_posix()
        for p in directory.rglob("*") if p.is_file() and p.name != "manifest.json"
    )
    assert [entry["path"] for entry in files] == actual, "manifest file set differs"
    for entry in files:
        path = Path(entry["path"])
        assert not path.is_absolute() and ".." not in path.parts
        assert hashlib.sha256((directory / path).read_bytes()).hexdigest() == entry["sha256"]
    encoded = json.dumps(files, ensure_ascii=False, separators=(",", ":")).encode()
    assert hashlib.sha256(encoded).hexdigest() == manifest["contract_hash"]
    assert "DaemonDiscovery" not in (directory / "index.ts").read_text()
    assert "DaemonDiscovery" in (directory / "node.ts").read_text()
    return manifest


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--typescript", action="store_true")
    parser.add_argument("--release", action="store_true", help="require a clean checkout and inject actual git revision")
    options = parser.parse_args()
    env = os.environ.copy()
    revision = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT, text=True).strip())
    env.update(QAQH_SOURCE_REVISION=revision, QAQH_SOURCE_DIRTY="1" if dirty else "0")
    if options.release and dirty:
        raise SystemExit("refusing release validation from a dirty checkout")
    if options.release:
        cargo = tomllib.loads((ROOT / "Cargo.toml").read_text())
        for name, dependency in cargo["workspace"]["dependencies"].items():
            if not isinstance(dependency, dict) or "path" not in dependency:
                continue
            source = (ROOT / dependency["path"]).resolve()
            if source.is_relative_to(ROOT):
                continue
            changes = subprocess.check_output(["git", "status", "--porcelain"], cwd=source, text=True)
            if changes.strip():
                raise SystemExit(f"refusing release validation with dirty external path dependency: {name}")
    suffix = ["--release"] if options.release else []
    run("cargo", "test", "-p", "qaqh-contract-export")
    with tempfile.TemporaryDirectory(prefix="qaqh-sdk-contract-check-") as temp:
        directory = Path(temp) / "contract"
        command = ["cargo", "run", "-p", "qaqh-contract-export", "--"]
        run(*command, "export", "--out", str(directory), *suffix, env=env)
        manifest = verify(directory)
        run(*command, "check", "--against", str(directory), *suffix, env=env)
        if options.typescript:
            compiler = shutil.which("tsc")
            if compiler is None:
                raise SystemExit("TypeScript verification requested but tsc is not installed; no implicit install is performed")
            (Path(temp) / "consumer.ts").write_text('''import type { RingingV2OpenRequest, RingingV2CommandEnvelope, RingingV2CommandStatus, ClientV2Bootstrap, ClientV2ControlState, RingingV2PendingApprovals, RingingV2ContentUploadResponse } from "./contract/index";
import type { ServiceMap } from "./contract/service-map";
import { serviceMetadata } from "./contract/service-map";
const open: RingingV2OpenRequest = { schema: "qaqh.Ringing", version: 2, client_instance_id: "synthetic" };
const params: ServiceMap["daemon.version"]["params"] = {};
const kind: "read" = serviceMetadata["daemon.version"].kind;
const siwcParams: ServiceMap["siwc.sign_in.status"]["params"] = { flow_id: "synthetic" };
const siwcResult: ServiceMap["siwc.status"]["result"] = { status: "connected", sharing: true, profiles: 1, usageUrl: "https://example.invalid", apiBase: "https://example.invalid/v1" };
const adminOnly: true = serviceMetadata["dsaccount.verify"].admin_only;
const driver: ClientV2ControlState["driver"] = { holder: null, driver_epoch: 1, can_claim: true };
const bootstrapDriver: ClientV2Bootstrap["control"]["state"]["driver"] = driver;
const approvals: RingingV2PendingApprovals = { pending_permission: null, pending_interaction: null };
const upload: RingingV2ContentUploadResponse["truncated"] = false;
const activityParams: ServiceMap["session.activity"]["params"] = {};
const activityEntry: ServiceMap["session.activity"]["result"][number] = { session_id: "synthetic", state: "working", turn_id: "turn-1", seq: 1, updated_at: 1 };
const activityNoTurn: ServiceMap["session.activity"]["result"][number] = { session_id: "synthetic", state: "idle", seq: 1, updated_at: 1 };
const setToolMode: ServiceMap["session.set_tool_mode"]["params"] = { session_id: "synthetic", tool_mode: "custom", custom_tools: ["fs_read"] };
const setProfile: ServiceMap["session.set_profile"]["params"] = { session_id: "synthetic", name: "" };
const workspaceEntry: ServiceMap["workspace.list"]["result"][number] = { id: "ws-1", path: "/synthetic/workspace", title: "workspace", order: 0, session_ids: ["0199a0f0-0000-7000-8000-000000000001"], missing_dir: false };
const workspaceCreate: ServiceMap["workspace.create"]["result"] = { id: "ws-1", path: "/synthetic/workspace", title: "workspace", order: 0, session_ids: [] };
const workspaceUnbound: ServiceMap["workspace.get"]["result"] = "";
void [open, params, kind, siwcParams, siwcResult, adminOnly, bootstrapDriver, approvals, upload, activityParams, activityEntry, activityNoTurn, setToolMode, setProfile, workspaceEntry, workspaceCreate, workspaceUnbound];
type Command = RingingV2CommandEnvelope;
type Status = RingingV2CommandStatus;
// @ts-expect-error unknown methods must not have a catch-all signature
type Missing = ServiceMap["not.a.method"];
// @ts-expect-error invalid filesystem parameter must be rejected
const invalid: ServiceMap["fs.read"]["params"] = {path: 1};
// @ts-expect-error account flow id is required
const missingFlow: ServiceMap["siwc.sign_in.status"]["params"] = {};
// @ts-expect-error account statuses are a closed enum
const badStatus: ServiceMap["siwc.status"]["result"] = { status: "maybe", sharing: true, profiles: 1, usageUrl: "u", apiBase: "a" };
// @ts-expect-error approval slots are required keys, not optional
const badApprovals: RingingV2PendingApprovals = {};
// @ts-expect-error activity state is a closed enum
const badActivityState: ServiceMap["session.activity"]["result"] = [{ session_id: "s", state: "paused", seq: 1, updated_at: 1 }];
// @ts-expect-error custom_tools entries must be strings
const badTools: ServiceMap["session.set_tool_mode"]["params"] = { session_id: "s", tool_mode: "custom", custom_tools: [1] };
// @ts-expect-error session.get_activity requires session_id
const missingSession: ServiceMap["session.get_activity"]["params"] = {};
// @ts-expect-error workspace list entries must carry missing_dir
const badWorkspaceEntry: ServiceMap["workspace.list"]["result"] = [{ id: "ws-1", path: "/s", title: "t", order: 0, session_ids: [] }];
// @ts-expect-error workspace.move_session requires workspace_id
const missingWorkspace: ServiceMap["workspace.move_session"]["params"] = { session_id: "s" };
''')
            config = {"compilerOptions": {"strict": True, "noEmit": True, "target": "ES2022", "module": "ESNext", "moduleResolution": "Bundler", "skipLibCheck": False}, "include": ["contract/**/*.ts", "consumer.ts"]}
            (Path(temp) / "tsconfig.json").write_text(json.dumps(config))
            run(compiler, "--project", str(Path(temp) / "tsconfig.json"))
        print(f"SDK contract verified: revision={revision} dirty={dirty} hash={manifest['contract_hash']}")


if __name__ == "__main__":
    main()
