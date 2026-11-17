// Exit criterion 5: run the typecheck and the driver against several 2.x releases of the MCP SDK.
// Each version gets a throwaway install in the OS temp directory, with the client package at the
// same version as the server package. Usage: node scripts/matrix.mjs [version ...]

import { execFileSync, spawnSync } from "node:child_process";
import { cpSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const root = fileURLToPath(new URL("..", import.meta.url));
const manifest = JSON.parse(readFileSync(join(root, "package.json"), "utf8"));
const versions =
  process.argv.length > 2 ? process.argv.slice(2) : ["2.0.0", "2.1.0", "2.2.0", "2.3.1", "2.4.0"];
const shell = process.platform === "win32";

const results = [];
for (const version of versions) {
  const dir = mkdtempSync(join(tmpdir(), `crucible-spike-matrix-${version}-`));
  cpSync(join(root, "src"), join(dir, "src"), { recursive: true });
  cpSync(join(root, "tsconfig.json"), join(dir, "tsconfig.json"));
  writeFileSync(
    join(dir, "package.json"),
    JSON.stringify({
      ...manifest,
      dependencies: {
        ...manifest.dependencies,
        "@modelcontextprotocol/client": version,
        "@modelcontextprotocol/server": version,
      },
    }),
  );
  execFileSync("npm", ["install", "--no-audit", "--no-fund", "--silent"], {
    cwd: dir,
    stdio: "inherit",
    shell,
  });
  const typecheck = spawnSync("npx", ["tsc", "--noEmit"], { cwd: dir, encoding: "utf8", shell });
  const driver = spawnSync(process.execPath, ["src/driver.ts"], { cwd: dir, encoding: "utf8" });
  const lines = driver.stdout.split("\n");
  const failed = lines.filter((line) => line.startsWith("FAIL"));
  results.push({
    version,
    typecheck: typecheck.status === 0 ? "ok" : "errors",
    passed: lines.filter((line) => line.startsWith("PASS")).length,
    failed: failed.length,
    exit: driver.status,
  });
  if (typecheck.status !== 0) {
    console.log(`--- ${version} typecheck\n${typecheck.stdout}${typecheck.stderr}`);
  }
  if (driver.status !== 0) {
    console.log(`--- ${version} driver\n${failed.join("\n")}\n${driver.stderr.slice(-2000)}`);
  }
  rmSync(dir, { recursive: true, force: true });
}

console.log("\nversion  typecheck  checks passed  checks failed  driver exit");
for (const r of results) {
  console.log(
    `${r.version.padEnd(8)} ${r.typecheck.padEnd(10)} ${String(r.passed).padEnd(14)} ${String(r.failed).padEnd(14)} ${r.exit}`,
  );
}
process.exit(results.every((r) => r.exit === 0 && r.typecheck === "ok") ? 0 : 1);
