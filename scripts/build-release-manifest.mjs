import { createHash } from "node:crypto";
import { readFileSync, readdirSync, unlinkSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const args = parseArgs(process.argv.slice(2));
const input = resolve(root, args.input || "dist");
const metadataFiles = readdirSync(input).filter(name => name.endsWith(".metadata.json"));
if (metadataFiles.length === 0) throw new Error(`No release metadata found in ${input}`);

const entries = metadataFiles.map(name => JSON.parse(readFileSync(join(input, name), "utf8")))
  .sort((left, right) => left.target.localeCompare(right.target));
const first = entries[0];
for (const entry of entries) {
  for (const field of ["version", "protocol_version", "git_commit"]) {
    if (entry[field] !== first[field]) throw new Error(`Release metadata disagrees on ${field}`);
  }
  const bytes = readFileSync(join(input, entry.artifact));
  const actual = createHash("sha256").update(bytes).digest("hex");
  if (actual !== entry.sha256) throw new Error(`Checksum mismatch for ${entry.artifact}`);
}

const repository = process.env.GITHUB_REPOSITORY || "QAQTam/DeepX";
const tag = process.env.GITHUB_REF_NAME || `v${first.version}`;
const baseUrl = `https://github.com/${repository}/releases/download/${tag}`;
const manifest = {
  schema_version: 1,
  version: first.version,
  protocol_version: first.protocol_version,
  git_commit: first.git_commit,
  repository,
  tag,
  artifacts: Object.fromEntries(entries.map(entry => [entry.target, {
    name: entry.artifact,
    url: `${baseUrl}/${entry.artifact}`,
    size: entry.size,
    sha256: entry.sha256,
  }])),
};

writeFileSync(join(input, "deepx-release.json"), `${JSON.stringify(manifest, null, 2)}\n`);
writeFileSync(join(input, "SHA256SUMS"), `${entries.map(entry => `${entry.sha256}  ${entry.artifact}`).join("\n")}\n`);
for (const name of metadataFiles) unlinkSync(join(input, name));
console.log(`Created release manifest for ${entries.length} target(s)`);

function parseArgs(values) {
  const parsed = {};
  for (let index = 0; index < values.length; index += 2) {
    const key = values[index]?.replace(/^--/, "");
    if (!key || !values[index + 1]) throw new Error(`Invalid argument near ${values[index] ?? "end"}`);
    parsed[key] = values[index + 1];
  }
  return parsed;
}
