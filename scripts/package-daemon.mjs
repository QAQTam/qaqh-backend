import { copyFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { basename, dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = parseArgs(process.argv.slice(2));
const targetId = required(args, "target-id");
const source = resolve(root, required(args, "binary"));
const output = resolve(root, args.output || "dist");
const version = capture(readFileSync(join(root, "Cargo.toml"), "utf8"), /\[workspace\.package\][\s\S]*?version\s*=\s*"([^"]+)"/, "workspace version");
const protocolVersion = Number(capture(readFileSync(join(root, "crates", "deepx-proto", "src", "control.rs"), "utf8"), /CONTROL_PROTOCOL_VERSION:\s*u16\s*=\s*(\d+)/, "control protocol version"));
const extension = targetId.startsWith("windows-") ? ".exe" : "";
const artifact = `deepx-daemon-v${version}-${targetId}${extension}`;
const destination = join(output, artifact);
const gitCommit = process.env.GITHUB_SHA || gitHead();

const tag = process.env.GITHUB_REF_NAME;
if (tag?.startsWith("v") && tag !== `v${version}`) {
  throw new Error(`Release tag ${tag} does not match Cargo version v${version}`);
}

mkdirSync(output, { recursive: true });
copyFileSync(source, destination);
const bytes = readFileSync(destination);
if (!targetId.startsWith("windows-")) {
  const { chmodSync } = await import("node:fs");
  chmodSync(destination, 0o755);
}

const metadata = {
  schema_version: 1,
  version,
  protocol_version: protocolVersion,
  git_commit: gitCommit,
  target: targetId,
  artifact,
  size: bytes.length,
  sha256: createHash("sha256").update(bytes).digest("hex"),
};
writeFileSync(`${destination}.metadata.json`, `${JSON.stringify(metadata, null, 2)}\n`);
console.log(`Packaged ${basename(source)} as ${artifact}`);

function gitHead() {
  const result = spawnSync("git", ["rev-parse", "HEAD"], { cwd: root, encoding: "utf8" });
  if (result.status !== 0) throw new Error("Unable to resolve the backend git commit");
  return result.stdout.trim();
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

function required(values, key) {
  if (!values[key]) throw new Error(`Missing --${key}`);
  return values[key];
}

function capture(content, pattern, label) {
  const value = content.match(pattern)?.[1];
  if (!value) throw new Error(`Unable to read ${label}`);
  return value;
}
