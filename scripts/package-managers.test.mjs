// Renders the formula and manifest from a fixture SHA256SUMS.
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdtempSync, writeFileSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { parseSums, formula, scoop, className } from "./package-managers.mjs";

const h = (c) => c.repeat(64);
const FIXTURE = [
  `${h("a")}  tool-darwin-arm64.tar.gz`,
  `${h("b")}  tool-darwin-x64.tar.gz`,
  `${h("c")}  tool-linux-arm64-gnu.tar.gz`,
  `${h("d")}  tool-linux-x64-gnu.tar.gz`,
  `${h("e")} *tool-win32-x64-msvc.zip`,
  `${h("f")}  tool-linux-x64-gnu.identity.json`,
  "",
].join("\n");
const meta = { name: "tool", version: "1.2.3", repo: "SylphxAI/my-tool", description: 'A "quoted" tool', homepage: "https://example.com", license: "MIT" };
const sums = parseSums(FIXTURE);

test("parseSums reads text and binary markers", () => {
  assert.equal(Object.keys(sums).length, 6);
  assert.equal(sums["tool-win32-x64-msvc.zip"], h("e"));
});

test("formula carries url and sha256 per OS and arch", () => {
  const f = formula({ ...meta, sums });
  assert.match(f, /^class Tool < Formula$/m);
  assert.match(f, /desc "A \\"quoted\\" tool"/);
  assert.match(f, /on_macos do\n    on_arm do\n      url "https:\/\/github.com\/SylphxAI\/my-tool\/releases\/download\/v1.2.3\/tool-darwin-arm64.tar.gz"\n      sha256 "a{64}"/);
  assert.match(f, /on_linux do\n    on_arm do\n      url ".*tool-linux-arm64-gnu.tar.gz"\n      sha256 "c{64}"\n    end\n\n    on_intel do\n      url ".*tool-linux-x64-gnu.tar.gz"\n      sha256 "d{64}"/);
  assert.match(f, /bin.install "tool"/);
  assert.match(f, /shell_output\("#\{bin\}\/tool version"\)/);
  assert.equal((f.match(/^\s*end$/gm) ?? []).length, 9);
});

test("formula refuses a missing archive digest", () => {
  const { ["tool-linux-x64-gnu.tar.gz"]: _, ...rest } = sums;
  assert.throws(() => formula({ ...meta, sums: rest }), /tool-linux-x64-gnu.tar.gz/);
});

test("scoop manifest has hash, bin, checkver and autoupdate", () => {
  const m = JSON.parse(scoop({ ...meta, sums }));
  assert.equal(m.version, "1.2.3");
  assert.equal(m.architecture["64bit"].hash, h("e"));
  assert.equal(m.architecture["64bit"].url, "https://github.com/SylphxAI/my-tool/releases/download/v1.2.3/tool-win32-x64-msvc.zip");
  assert.equal(m.bin, "tool.exe");
  assert.equal(m.checkver.github, "https://github.com/SylphxAI/my-tool");
  assert.equal(m.autoupdate.architecture["64bit"].url, "https://github.com/SylphxAI/my-tool/releases/download/v$version/tool-win32-x64-msvc.zip");
});

test("class names", () => {
  assert.equal(className("mcp-kit"), "McpKit");
  assert.equal(className("anymd"), "Anymd");
});

test("CLI writes both files, metadata defaults from package.json", () => {
  const d = mkdtempSync(join(tmpdir(), "pm-"));
  writeFileSync(join(d, "SHA256SUMS"), FIXTURE);
  writeFileSync(join(d, "package.json"), JSON.stringify({ description: "From npm", license: "Apache-2.0" }));
  execFileSync("node", [resolve(import.meta.dirname, "package-managers.mjs"), "--name", "tool", "--version", "1.2.3", "--repo", "SylphxAI/my-tool", "--sums", join(d, "SHA256SUMS"), "--package-json", join(d, "package.json"), "--formula-out", join(d, "Formula/tool.rb"), "--scoop-out", join(d, "bucket/tool.json")]);
  const f = readFileSync(join(d, "Formula/tool.rb"), "utf8");
  assert.match(f, /desc "From npm"/);
  assert.match(f, /license "Apache-2.0"/);
  assert.match(f, /homepage "https:\/\/github.com\/SylphxAI\/my-tool"/);
  assert.equal(JSON.parse(readFileSync(join(d, "bucket/tool.json"), "utf8")).license, "Apache-2.0");
});
