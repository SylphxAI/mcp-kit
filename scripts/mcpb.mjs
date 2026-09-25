#!/usr/bin/env node
// Build MCP Bundles (.mcpb) for an mcp-kit server: one per platform and one
// for all platforms. Spec: github.com/modelcontextprotocol/mcpb (manifest 0.3).
//
//   node mcpb.mjs --name repomap --version 1.3.0 --natives packages/npm \
//     --package-dir packages/repomap --out assets [--icon icon.png] [--tools-from <binary>]
//
// Reads server.json (title, description, website, arguments) and the npm
// package (license, keywords, author). Lists the tools by starting the server
// once. An optional mcpb.json at the repository root is merged into every
// manifest last (for user_config, extra env, and so on).
//
// Output: <name>-<version>-<platform>.mcpb (binary server, one per native
// package) and <name>-<version>.mcpb (all binaries plus a small Node launcher,
// run by the host's own Node; Claude Desktop ships one).
import { spawn, execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const MCPB_CLI = "@anthropic-ai/mcpb@2.1.2";

function args() {
  const a = {};
  const v = process.argv.slice(2);
  for (let i = 0; i < v.length; i += 2) a[v[i].replace(/^--/, "")] = v[i + 1];
  for (const k of ["name", "version", "natives", "out"]) if (!a[k]) throw new Error(`--${k} is required`);
  a["package-dir"] ??= `packages/${a.name}`;
  return a;
}

const readJson = (p) => JSON.parse(readFileSync(p, "utf8"));

export function platformOf(key) {
  const os = key.split("-")[0];
  if (!["darwin", "linux", "win32"].includes(os)) throw new Error(`unknown platform in ${key}`);
  return os;
}

/** Deep merge: objects merge, everything else replaces. */
export function merge(base, over) {
  if (!over || typeof over !== "object" || Array.isArray(over)) return over ?? base;
  const out = { ...base };
  for (const [k, v] of Object.entries(over)) out[k] = v && typeof v === "object" && !Array.isArray(v) && base?.[k] && typeof base[k] === "object" ? merge(base[k], v) : v;
  return out;
}

/** Start the server, ask for tools/list over stdio, and stop it. */
export async function listTools(bin, argv) {
  const p = spawn(bin, argv, { stdio: ["pipe", "pipe", "inherit"] });
  const send = (m) => p.stdin.write(JSON.stringify(m) + "\n");
  let buf = "";
  const tools = await new Promise((ok, fail) => {
    const timer = setTimeout(() => fail(new Error("no tools/list answer in 20 s")), 20000);
    p.stdout.on("data", (d) => {
      buf += d;
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const line = buf.slice(0, i);
        buf = buf.slice(i + 1);
        let m;
        try { m = JSON.parse(line); } catch { continue; }
        if (m.id === 1) {
          send({ jsonrpc: "2.0", method: "notifications/initialized" });
          send({ jsonrpc: "2.0", id: 2, method: "tools/list", params: {} });
        } else if (m.id === 2) {
          clearTimeout(timer);
          ok(m.result?.tools ?? []);
        }
      }
    });
    p.on("error", fail);
    send({ jsonrpc: "2.0", id: 1, method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "mcpb", version: "1" } } });
  });
  p.kill();
  // The manifest lists names and short descriptions: the first sentence, or
  // the first two when the first is very short ("Start here.").
  return tools.map((t) => {
    const s = (t.description ?? "").split(/(?<=\.)\s+/);
    return { name: t.name, description: s[0].length < 40 && s[1] ? `${s[0]} ${s[1]}` : s[0] };
  });
}

export function manifest({ name, version, server, pkg, tools, platforms, binary, extra }) {
  const argv = (server.packages?.[0]?.packageArguments ?? []).map((a) => a.value).filter(Boolean);
  const repo = server.repository?.url ?? pkg.repository?.url?.replace(/^git\+/, "").replace(/\.git$/, "");
  const author = typeof pkg.author === "string" ? { name: pkg.author } : pkg.author ?? { name: repo?.split("/")[3] ?? name };
  const m = {
    manifest_version: "0.3",
    name,
    display_name: server.title ?? name,
    version,
    description: server.description ?? pkg.description,
    author,
    ...(repo && { repository: { type: "git", url: repo }, support: `${repo}/issues` }),
    ...(server.websiteUrl && { homepage: server.websiteUrl, documentation: server.websiteUrl }),
    ...(extra.icon && { icon: "icon.png" }),
    server: binary
      ? {
          type: "binary",
          entry_point: `server/${binary}`,
          mcp_config: { command: `\${__dirname}/server/${binary}`, args: argv, env: {} },
        }
      : {
          type: "node",
          entry_point: "server/index.js",
          mcp_config: { command: "node", args: ["${__dirname}/server/index.js", ...argv], env: {} },
        },
    tools,
    tools_generated: false,
    keywords: pkg.keywords ?? [],
    license: pkg.license ?? "MIT",
    compatibility: { platforms },
  };
  return merge(m, extra.overrides);
}

// Picks the binary for this machine from server/bin/<platform-arch>/.
const LAUNCHER = `#!/usr/bin/env node
const { spawn } = require("node:child_process");
const { existsSync } = require("node:fs");
const { join } = require("node:path");
const NAME = __NAME__;
const KEYS = __KEYS__;
const key = KEYS.find((k) => k.startsWith(process.platform + "-" + process.arch));
const bin = key && join(__dirname, "bin", key, NAME + (process.platform === "win32" ? ".exe" : ""));
if (!bin || !existsSync(bin)) {
  console.error(NAME + ": no binary for " + process.platform + "-" + process.arch + " in this bundle");
  process.exit(1);
}
const child = spawn(bin, process.argv.slice(2), { stdio: "inherit", windowsHide: true });
for (const s of ["SIGINT", "SIGTERM", "SIGHUP"]) process.on(s, () => child.kill(s));
child.on("exit", (code, signal) => (signal ? process.kill(process.pid, signal) : process.exit(code ?? 1)));
`;

function pack(dir, file) {
  execFileSync("npx", ["-y", MCPB_CLI, "pack", dir, file], { stdio: "inherit" });
}

async function main() {
  const a = args();
  const server = readJson("server.json");
  const pkg = readJson(join(a["package-dir"], "package.json"));
  const extra = { icon: a.icon, overrides: existsSync("mcpb.json") ? readJson("mcpb.json") : undefined };
  const keys = readdirSync(a.natives).filter((k) => existsSync(join(a.natives, k, "package.json"))).sort();
  const exe = (k) => a.name + (platformOf(k) === "win32" ? ".exe" : "");
  const argv = (server.packages?.[0]?.packageArguments ?? []).map((x) => x.value).filter(Boolean);
  const toolsFrom = a["tools-from"] ?? keys.map((k) => join(a.natives, k, exe(k))).find(existsSync);
  const tools = await listTools(resolve(toolsFrom), argv);
  if (!tools.length) throw new Error("the server listed no tools");
  mkdirSync(a.out, { recursive: true });
  const work = mkdtempSync(join(tmpdir(), "mcpb-"));
  const stage = (dir, m) => {
    mkdirSync(join(dir, "server"), { recursive: true });
    writeFileSync(join(dir, "manifest.json"), JSON.stringify(m, null, 2) + "\n");
    if (a.icon) cpSync(a.icon, join(dir, "icon.png"));
    for (const f of ["LICENSE", "README.md"]) if (existsSync(f)) cpSync(f, join(dir, f));
  };
  // One bundle per platform: the binary alone.
  for (const k of keys) {
    const dir = join(work, k);
    stage(dir, manifest({ name: a.name, version: a.version, server, pkg, tools, platforms: [platformOf(k)], binary: exe(k), extra }));
    cpSync(join(a.natives, k, exe(k)), join(dir, "server", exe(k)));
    chmodSync(join(dir, "server", exe(k)), 0o755);
    pack(dir, join(a.out, `${a.name}-${a.version}-${k}.mcpb`));
  }
  // One bundle for every platform: all binaries and the launcher.
  const dir = join(work, "all");
  const platforms = [...new Set(keys.map(platformOf))];
  stage(dir, manifest({ name: a.name, version: a.version, server, pkg, tools, platforms, binary: null, extra }));
  for (const k of keys) {
    mkdirSync(join(dir, "server", "bin", k), { recursive: true });
    cpSync(join(a.natives, k, exe(k)), join(dir, "server", "bin", k, exe(k)));
    chmodSync(join(dir, "server", "bin", k, exe(k)), 0o755);
  }
  writeFileSync(join(dir, "server", "index.js"), LAUNCHER.replace("__NAME__", JSON.stringify(a.name)).replace("__KEYS__", JSON.stringify(keys)));
  pack(dir, join(a.out, `${a.name}-${a.version}.mcpb`));
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main().catch((e) => {
    console.error(`mcpb: ${e.message}`);
    process.exit(1);
  });
}
