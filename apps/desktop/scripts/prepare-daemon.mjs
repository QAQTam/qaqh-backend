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
const destination = join(projectRoot, "build", "sidecar", executable);

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
writeFileSync(join(dirname(destination), "daemon-manifest.json"), `${JSON.stringify({
  version: lock.version,
  protocol_version: lock.protocol_version,
  build_id: stagedBuildId,
  channel: "stable",
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
