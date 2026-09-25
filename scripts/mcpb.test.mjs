// Builds bundles for a fake server and checks them with the official MCPB CLI.
import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { mkdirSync, mkdtempSync, writeFileSync, chmodSync, readFileSync, readdirSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { merge, platformOf, manifest } from "./mcpb.mjs";

const SCRIPT = resolve(import.meta.dirname, "mcpb.mjs");

// A tiny MCP server over stdio: answers initialize and tools/list.
const FAKE = `#!/usr/bin/env node
const rl = require("readline").createInterface({ input: process.stdin });
rl.on("line", (l) => {
  const m = JSON.parse(l);
  const reply = (result) => process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: m.id, result }) + "\\n");
  if (m.method === "initialize") reply({ protocolVersion: "2025-06-18", capabilities: { tools: {} }, serverInfo: { name: "tool", version: "1.0.0" } });
  if (m.method === "tools/list") reply({ tools: [{ name: "echo", description: "Echo. Returns the text it gets, unchanged." }] });
});
`;

test("merge and platforms", () => {
  assert.deepEqual(merge({ a: { b: 1, c: 2 }, d: [1] }, { a: { c: 3 }, d: [2] }), { a: { b: 1, c: 3 }, d: [2] });
  assert.equal(platformOf("linux-arm64-gnu"), "linux");
  assert.throws(() => platformOf("plan9-x64"));
  const m = manifest({
    name: "tool", version: "1.0.0", server: { title: "Tool", description: "d", packages: [{ packageArguments: [{ value: "mcp" }] }] },
    pkg: { license: "MIT" }, tools: [], platforms: ["win32"], binary: "tool.exe", extra: {},
  });
  assert.equal(m.server.mcp_config.command, "${__dirname}/server/tool.exe");
  assert.deepEqual(m.server.mcp_config.args, ["mcp"]);
});

test("bundles for every platform and one for all", () => {
  const d = mkdtempSync(join(tmpdir(), "mcpb-test-"));
  const keys = ["darwin-arm64", "darwin-x64", "linux-arm64-gnu", "linux-x64-gnu", "win32-x64-msvc"];
  for (const k of keys) {
    mkdirSync(join(d, "packages/npm", k), { recursive: true });
    writeFileSync(join(d, "packages/npm", k, "package.json"), "{}");
    const bin = join(d, "packages/npm", k, k.startsWith("win32") ? "tool.exe" : "tool");
    writeFileSync(bin, FAKE);
    chmodSync(bin, 0o755);
  }
  mkdirSync(join(d, "packages/tool"), { recursive: true });
  writeFileSync(join(d, "packages/tool/package.json"), JSON.stringify({ name: "@x/tool", license: "MIT", keywords: ["mcp"] }));
  writeFileSync(join(d, "server.json"), JSON.stringify({ title: "Tool", description: "A test tool.", repository: { url: "https://github.com/x/tool" }, packages: [{ packageArguments: [{ type: "positional", value: "mcp" }] }] }));
  writeFileSync(join(d, "mcpb.json"), JSON.stringify({ user_config: { project: { type: "directory", title: "Project", description: "Folder", required: true } } }));
  execFileSync("node", [SCRIPT, "--name", "tool", "--version", "1.2.3", "--natives", "packages/npm", "--out", "assets"], { cwd: d, stdio: "inherit" });
  const out = readdirSync(join(d, "assets")).sort();
  assert.deepEqual(out, ["tool-1.2.3-darwin-arm64.mcpb", "tool-1.2.3-darwin-x64.mcpb", "tool-1.2.3-linux-arm64-gnu.mcpb", "tool-1.2.3-linux-x64-gnu.mcpb", "tool-1.2.3-win32-x64-msvc.mcpb", "tool-1.2.3.mcpb"]);
  for (const f of ["tool-1.2.3.mcpb", "tool-1.2.3-win32-x64-msvc.mcpb"]) {
    const x = join(d, f + ".d");
    execFileSync("unzip", ["-q", join(d, "assets", f), "-d", x]);
    execFileSync("npx", ["-y", "@anthropic-ai/mcpb@2.1.2", "validate", join(x, "manifest.json")], { stdio: "inherit" });
    const m = JSON.parse(readFileSync(join(x, "manifest.json"), "utf8"));
    assert.equal(m.version, "1.2.3");
    assert.deepEqual(m.tools, [{ name: "echo", description: "Echo. Returns the text it gets, unchanged." }]);
    assert.equal(m.user_config.project.type, "directory");
  }
  // The launcher in the all-platform bundle starts this machine's binary.
  const all = join(d, "tool-1.2.3.mcpb.d");
  const init = JSON.stringify({ jsonrpc: "2.0", id: 1, method: "initialize", params: {} });
  const reply = execFileSync("node", [join(all, "server/index.js"), "mcp"], { input: init + "\n" }).toString();
  assert.match(reply, /"serverInfo"/);
  assert.equal(JSON.parse(readFileSync(join(d, "tool-1.2.3-win32-x64-msvc.mcpb.d/manifest.json"), "utf8")).server.entry_point, "server/tool.exe");
});
