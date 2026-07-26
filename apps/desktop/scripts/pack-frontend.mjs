import { createPackageWithOptions } from "@electron/asar";
import {
  cpSync,
  existsSync,
  mkdirSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const outputRoot = join(projectRoot, "release", "frontend");
const stageRoot = join(projectRoot, "release", ".frontend-stage");
const outDir = join(projectRoot, "out");
const wsDir = join(projectRoot, "node_modules", "ws");

if (!existsSync(outDir)) {
  throw new Error(`Frontend output not found: ${outDir}. Run pnpm build first.`);
}
if (!existsSync(wsDir)) {
  throw new Error(`Runtime dependency not found: ${wsDir}. Run pnpm install first.`);
}

rmSync(stageRoot, { recursive: true, force: true });
rmSync(outputRoot, { recursive: true, force: true });
mkdirSync(join(stageRoot, "node_modules"), { recursive: true });
mkdirSync(outputRoot, { recursive: true });

cpSync(outDir, join(stageRoot, "out"), { recursive: true, dereference: true });
cpSync(wsDir, join(stageRoot, "node_modules", "ws"), {
  recursive: true,
  dereference: true,
});

const packageJson = JSON.parse(
  readFileSync(join(projectRoot, "package.json"), "utf8"),
);
writeFileSync(
  join(stageRoot, "package.json"),
  `${JSON.stringify(
    {
      name: packageJson.name,
      version: packageJson.version,
      description: packageJson.description,
      main: packageJson.main,
      type: packageJson.type,
      dependencies: { ws: packageJson.dependencies.ws },
    },
    null,
    2,
  )}\n`,
);

const asarPath = join(outputRoot, "app.asar");
await createPackageWithOptions(stageRoot, asarPath, {
  dot: true,
  unpack: "*.node",
});

rmSync(stageRoot, { recursive: true, force: true });
console.log(`Packed frontend ASAR: ${asarPath}`);
