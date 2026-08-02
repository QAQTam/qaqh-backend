import { copyFileSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const lock = JSON.parse(readFileSync(join(projectRoot, "deepx-backend.lock.json"), "utf8"));
const args = parseArgs(process.argv.slice(2));
const explicitBackend = args["backend-root"] || process.env.DEEPX_BACKEND_ROOT;
const targetId = resolveTarget();
const executable = process.platform === "win32" ? "deepx-daemon.exe" : "deepx-daemon";
const workspaceExecutable =
  process.platform === "win32" ? "deepx-workspace.exe" : "deepx-workspace";
const destination = join(projectRoot, "build", "sidecar", executable);
const workspaceDestination = join(projectRoot, "build", "sidecar", workspaceExecutable);

validateDesktopProtocol();
const desktopVersion = JSON.parse(readFileSync(join(projectRoot, "package.json"), "utf8").toString()).version;
if (desktopVersion !== lock.version) throw new Error(`Desktop version ${desktopVersion} does not match backend lock ${lock.version}`);
if (process.env.GITHUB_REF_NAME?.startsWith("v") && process.env.GITHUB_REF_NAME !== `v${desktopVersion}`) {
  throw new Error(`Release tag ${process.env.GITHUB_REF_NAME} does not match Desktop version v${desktopVersion}`);
}
mkdirSync(dirname(destination), { recursive: true });

const stagedBuildId = explicitBackend
  ? await stageLocalBackend(resolve(explicitBackend))
  : await stageReleaseArtifact();
// 校验预置 daemon 实际嵌入的 build_id 与清单一致，避免把
// git 不可用/构建缓存陈旧导致 build_id 回退到版本的二进制打进包。
verifyDaemonBuildId(destination, stagedBuildId);
// workspace 二进制与 daemon 同源构建（just build-daemon 一起产出）。
// Full 包与 Backend 包都会携带它；daemon 负责拉起（未随包时为可选项）。
if (explicitBackend) {
  const workspaceSource = join(resolve(explicitBackend), "target", "release", workspaceExecutable);
  if (!existsSync(workspaceSource)) {
    throw new Error(
      `Pre-built workspace binary not found at ${workspaceSource}. Run 'just build-daemon' (builds both).`,
    );
  }
  copyFileSync(workspaceSource, workspaceDestination);
  console.log(`Staged local workspace ${workspaceSource} -> ${workspaceDestination}`);
}
// workspace 二进制 SHA-256：daemon 拉起前校验完整性（防篡改回传）。
const workspaceSha256 = sha256(readFileSync(workspaceDestination));
writeFileSync(join(dirname(destination), "daemon-manifest.json"), `${JSON.stringify({
  version: lock.version,
  protocol_version: lock.protocol_version,
  build_id: stagedBuildId,
  channel: "stable",
  workspace: explicitBackend ? "bundled" : "release",
  workspace_sha256: workspaceSha256,
}, null, 2)}\n`);

async function stageLocalBackend(backendRoot) {
  const cargoToml = join(backendRoot, "Cargo.toml");
  if (!existsSync(cargoToml)) throw new Error(`DeepX backend was not found at ${backendRoot}`);
  const backendVersion = capture(readFileSync(cargoToml, "utf8"), /\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"/, "backend version");
  const backendProtocol = Number(capture(readFileSync(join(backendRoot, "crates", "deepx-proto", "src", "control.rs"), "utf8"), /CONTROL_PROTOCOL_VERSION:\s*u16\s*=\s*(\d+)/, "backend protocol"));
  if (backendVersion !== lock.version || backendProtocol !== lock.protocol_version) {
    throw new Error(`Local backend ${backendVersion}/protocol ${backendProtocol} does not match lock ${lock.version}/protocol ${lock.protocol_version}`);
  }

  // daemon build is handled by the monorepo justfile (just build-daemon).
  // here we only copy the pre-built binary.
  const source = join(backendRoot, "target", "release", executable);
  if (!existsSync(source)) {
    throw new Error(`Pre-built daemon not found at ${source}. Run 'just build-daemon' first.`);
  }
  copyFileSync(source, destination);
  console.log(`Staged local backend ${source} -> ${destination}`);
  return gitCommit(backendRoot);
}

async function stageReleaseArtifact() {
  const response = await fetch(lock.release_manifest_url, { redirect: "follow" });
  if (!response.ok) throw new Error(`Unable to download backend manifest: HTTP ${response.status} ${response.statusText}`);
  const manifest = await response.json();
  for (const field of ["version", "protocol_version", "git_commit"]) {
    if (manifest[field] !== lock[field]) throw new Error(`Backend manifest ${field} does not match deepx-backend.lock.json`);
  }
  const artifact = manifest.artifacts?.[targetId];
  if (!artifact?.url || !artifact?.sha256 || !artifact?.name) throw new Error(`Backend release has no ${targetId} artifact`);

  // workspace 二进制（deepx-workspace serve）随 daemon 同版本发布；
  // release manifest 缺失时 fail fast，避免打出不带工具服务的残缺包。
  const workspaceArtifact = manifest.artifacts?.[`${targetId}-workspace`];
  if (!workspaceArtifact?.url || !workspaceArtifact?.sha256 || !workspaceArtifact?.name) {
    throw new Error(
      `Backend release has no ${targetId}-workspace artifact; publish deepx-workspace with the backend first.`,
    );
  }
  const workspaceCacheDir = join(projectRoot, ".cache", "deepx", workspaceArtifact.sha256);
  const workspaceCached = join(workspaceCacheDir, workspaceArtifact.name);
  mkdirSync(workspaceCacheDir, { recursive: true });
  if (!existsSync(workspaceCached) || sha256(readFileSync(workspaceCached)) !== workspaceArtifact.sha256) {
    const download = await fetch(workspaceArtifact.url, { redirect: "follow" });
    if (!download.ok) throw new Error(`Unable to download ${workspaceArtifact.name}: HTTP ${download.status} ${download.statusText}`);
    const bytes = Buffer.from(await download.arrayBuffer());
    if (sha256(bytes) !== workspaceArtifact.sha256) throw new Error(`Checksum mismatch for ${workspaceArtifact.name}`);
    writeFileSync(workspaceCached, bytes);
  }
  copyFileSync(workspaceCached, workspaceDestination);
  if (process.platform !== "win32") {
    const { chmodSync } = await import("node:fs");
    chmodSync(workspaceDestination, 0o755);
  }
  console.log(`Staged locked workspace ${lock.version} for ${targetId}`);

  const cacheDir = join(projectRoot, ".cache", "deepx", artifact.sha256);
  const cached = join(cacheDir, artifact.name);
  mkdirSync(cacheDir, { recursive: true });
  if (!existsSync(cached) || sha256(readFileSync(cached)) !== artifact.sha256) {
    const download = await fetch(artifact.url, { redirect: "follow" });
    if (!download.ok) throw new Error(`Unable to download ${artifact.name}: HTTP ${download.status} ${download.statusText}`);
    const bytes = Buffer.from(await download.arrayBuffer());
    if (sha256(bytes) !== artifact.sha256) throw new Error(`Checksum mismatch for ${artifact.name}`);
    writeFileSync(cached, bytes);
  }
  copyFileSync(cached, destination);
  if (process.platform !== "win32") {
    const { chmodSync } = await import("node:fs");
    chmodSync(destination, 0o755);
  }
  console.log(`Staged locked backend ${lock.version} (${lock.git_commit.slice(0, 12)}) for ${targetId}`);
  return lock.git_commit;
}

function gitCommit(backendRoot) {
  const result = spawnSync("git", ["rev-parse", "HEAD"], {
    cwd: backendRoot,
    encoding: "utf8",
    shell: false,
  });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Unable to resolve backend git commit: ${result.stderr.trim()}`);
  return result.stdout.trim();
}

/**
 * 校验预置 daemon 二进制实际嵌入的 build_id 与清单将声明的值一致。
 * build.rs 会把 DEEPX_BUILD_ID（默认 git commit）编译为字符串字面量写入
 * 二进制；git 不可用或构建产物陈旧时它会回退为 CARGO_PKG_VERSION，
 * 导致运行时 "incompatible daemon: build ... (expected ...)" 且设置页
 * 无法加载。因此这里必须 fail fast。直接扫描文件字节，避免运行 daemon
 * 派生 deepx-workspace 子进程造成残留。
 */
function verifyDaemonBuildId(executable, expectedBuildId) {
  const bytes = readFileSync(executable);
  if (!bytes.includes(Buffer.from(expectedBuildId, "utf8"))) {
    throw new Error(
      `staged daemon does not embed build ${expectedBuildId}; ` +
      `the daemon binary is stale or was built without git (build.rs fell back to CARGO_PKG_VERSION). ` +
      `Run 'just build-daemon' (or 'cargo clean -p deepx-daemon && cargo build --release -p deepx-daemon') and retry.`,
    );
  }
}

function validateDesktopProtocol() {
  const source = readFileSync(join(projectRoot, "electron", "controlClient.ts"), "utf8");
  const protocol = Number(capture(source, /PROTOCOL_VERSION\s*=\s*(\d+)/, "Desktop protocol"));
  if (protocol !== lock.protocol_version) throw new Error(`Desktop protocol ${protocol} does not match backend lock ${lock.protocol_version}`);
}

function resolveTarget() {
  const platform = { win32: "windows", linux: "linux", darwin: "macos" }[process.platform];
  const architecture = { x64: "x86_64", arm64: "arm64" }[process.arch];
  if (!platform || !architecture) throw new Error(`Unsupported packaging target ${process.platform}-${process.arch}`);
  return `${platform}-${architecture}`;
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function parseArgs(values) {
  const parsed = {};
  for (let index = 0; index < values.length; index += 2) {
    const key = values[index]?.replace(/^--/, "");
    if (!key || !values[index + 1]) throw new Error(`Invalid argument near ${values[index] ?? "end"}`);
    parsed[key] = values[index + 1];
  }
  return parsed;
}

function capture(content, pattern, label) {
  const value = content.match(pattern)?.[1];
  if (!value) throw new Error(`Unable to read ${label}`);
  return value;
}
