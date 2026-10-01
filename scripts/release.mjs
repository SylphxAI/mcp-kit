// Release planning and identity checks shared by the reusable workflow.
// Only HTTP 404 means absent. Everything else must return the exact identity.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync, writeFileSync, readdirSync, mkdirSync, copyFileSync, chmodSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

export const KEYS = ['darwin-arm64', 'darwin-x64', 'linux-x64-gnu', 'linux-arm64-gnu', 'win32-x64-msvc'];
const json = (file) => JSON.parse(readFileSync(file, 'utf8'));
const hash = (bytes, algorithm = 'sha256') => createHash(algorithm).update(bytes).digest();
const encode = encodeURIComponent;
const binary = (name, key) => name + (key.startsWith('win32') ? '.exe' : '');
const archive = (name, key) => `${name}-${key}.${key.startsWith('win32') ? 'zip' : 'tar.gz'}`;

export async function request(url, headers = {}, fetcher = fetch) {
  const response = await fetcher(url, { headers, signal: AbortSignal.timeout(30000) });
  if (response.status === 404) return null;
  if (response.status !== 200) throw new Error(`release probe failed: HTTP ${response.status} at ${new URL(url).origin}`);
  return response;
}

export async function npm(name, version, fetcher = fetch) {
  const response = await request(`https://registry.npmjs.org/${encode(name)}/${encode(version)}`, {}, fetcher);
  if (!response) return null;
  const value = await response.json();
  if (value.name !== name || value.version !== version || !value.dist?.integrity || !value.dist?.tarball) {
    throw new Error(`npm identity mismatch for ${name}@${version}`);
  }
  return value;
}

export async function registry(name, version, fetcher = fetch) {
  const response = await request(`https://registry.modelcontextprotocol.io/v0.1/servers/${encode(name)}/versions/${encode(version)}`, {}, fetcher);
  if (!response) return null;
  const value = await response.json();
  if (value.server?.name !== name || (version !== 'latest' && (value.server?.version !== version || value._meta?.['io.modelcontextprotocol.registry/official']?.status !== 'active'))) {
    throw new Error(`MCP Registry identity mismatch or inactive version for ${name}@${version}`);
  }
  return value;
}

export async function crate(name, version, fetcher = fetch) {
  const response = await request(`https://crates.io/api/v1/crates/${encode(name)}/${encode(version)}`, { 'User-Agent': 'mcp-kit-release (github.com/SylphxAI/mcp-kit)' }, fetcher);
  if (!response) return null;
  const value = await response.json();
  if (value.version?.crate !== name || value.version?.num !== version || value.version?.yanked) {
    throw new Error(`crates.io identity mismatch or yanked version: ${name}@${version}`);
  }
  return value;
}

const githubHeaders = () => ({ Accept: 'application/vnd.github+json', Authorization: `Bearer ${process.env.GH_TOKEN}`, 'X-GitHub-Api-Version': '2022-11-28' });
export async function github(repo, version, fetcher = fetch) {
  const response = await request(`https://api.github.com/repos/${repo}/releases/tags/${encode(`v${version}`)}`, githubHeaders(), fetcher);
  if (!response) return null;
  const value = await response.json();
  if (value.tag_name !== `v${version}` || value.draft || !Array.isArray(value.assets)) throw new Error('GitHub release identity mismatch');
  return value;
}

export async function image(name, version, fetcher = fetch, canonical, natives = {}, legacyComplete = false) {
  if (!name) return null;
  if (!name.startsWith('ghcr.io/') || !canonical) throw new Error('GHCR requires canonical release identity');
  const repository = name.slice('ghcr.io/'.length);
  // Authorized package metadata positively distinguishes a first publication
  // from an anonymous token denial. No non-404 is treated as absence.
  const [owner, ...path] = repository.split('/');
  const ownerResponse = await request(`https://api.github.com/users/${owner}`, githubHeaders(), fetcher);
  if (!ownerResponse) throw new Error('GHCR owner is absent');
  const ownerKind = (await ownerResponse.json()).type;
  if (!['Organization', 'User'].includes(ownerKind)) throw new Error('GHCR owner identity is invalid');
  const metadata = await request(`https://api.github.com/${ownerKind === 'Organization' ? 'orgs' : 'users'}/${owner}/packages/container/${encode(path.join('/'))}`, githubHeaders(), fetcher);
  if (!metadata) return null;
  const tokenResponse = await request(`https://ghcr.io/token?service=ghcr.io&scope=${encode(`repository:${repository}:pull`)}`, {
    Authorization: `Basic ${Buffer.from(`${process.env.GITHUB_ACTOR}:${process.env.GH_TOKEN}`).toString('base64')}`,
  }, fetcher);
  if (!tokenResponse) throw new Error('GHCR token endpoint is absent');
  const { token } = await tokenResponse.json();
  if (!token) throw new Error('GHCR did not provide a pull token');
  const headers = { Authorization: `Bearer ${token}`, Accept: 'application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json' };
  const load = async (path, expected) => {
    const response = await request(`https://ghcr.io/v2/${repository}/${path}`, headers, fetcher);
    if (!response) return null;
    const bytes = Buffer.from(await response.arrayBuffer());
    const digest = `sha256:${hash(bytes).toString('hex')}`;
    if ((expected && digest !== expected) || (!expected && response.headers.get('docker-content-digest') !== digest)) throw new Error('GHCR content digest mismatch');
    return { value: JSON.parse(bytes), digest };
  };
  const index = await load(`manifests/${encode(version)}`);
  if (!index) return null;
  if (legacyComplete) {
    // A pre-identity image is an existing channel record, not new build provenance.
    // Bind its exact version tag to the immutable index recorded by GitHub Packages.
    let recorded = false;
    for (let page = 1; !recorded; page++) {
      const versions = await request(`https://api.github.com/${ownerKind === 'Organization' ? 'orgs' : 'users'}/${owner}/packages/container/${encode(path.join('/'))}/versions?per_page=100&page=${page}`, githubHeaders(), fetcher);
      const records = versions && await versions.json();
      if (!Array.isArray(records)) throw new Error('legacy GHCR version records missing');
      recorded = records.some((record) => record.name === index.digest && record.metadata?.container?.tags?.includes(version));
      if (records.length < 100) break;
    }
    if (!recorded) throw new Error('legacy GHCR version record mismatch');
  }
  for (const arch of ['amd64', 'arm64']) {
    const descriptor = index.value.manifests?.find((m) => m.platform?.os === 'linux' && m.platform?.architecture === arch);
    if (!descriptor || !/^sha256:[a-f0-9]{64}$/.test(descriptor.digest)) throw new Error(`GHCR image lacks linux/${arch}`);
    const child = await load(`manifests/${descriptor.digest}`, descriptor.digest);
    if (!child || !/^sha256:[a-f0-9]{64}$/.test(child.value.config?.digest)) throw new Error('GHCR child manifest missing');
    const config = await load(`blobs/${child.value.config.digest}`, child.value.config.digest);
    const labels = config?.value.config?.Labels;
    const key = arch === 'amd64' ? 'linux-x64-gnu' : 'linux-arm64-gnu';
    if (legacyComplete) {
      if (config?.value.os !== 'linux' || config.value.architecture !== arch || (labels?.['org.opencontainers.image.version'] && labels['org.opencontainers.image.version'] !== version)) throw new Error('legacy GHCR image identity mismatch');
      continue;
    }
    if (config?.value.os !== 'linux' || config.value.architecture !== arch || labels?.['org.opencontainers.image.version'] !== version || labels?.['org.opencontainers.image.revision'] !== canonical.commit || labels?.['org.opencontainers.image.source'] !== `https://github.com/${canonical.repository}` || labels?.[`io.sylphx.native.${key}.sha256`] !== natives[key]?.sha256 || !natives[key]) throw new Error('GHCR image identity mismatch');
  }
  return { digest: index.digest, verification: legacyComplete ? 'existing-version-record' : 'native-identity' };
}

export function sourceIdentity(source) {
  if (!source || !/^[a-f0-9]{40}$/.test(source.commit) || !/^[^/]+\/[^/]+$/.test(source.repository)) throw new Error('invalid canonical source identity');
  return source;
}
export function sameSource(a, b) {
  sourceIdentity(a); sourceIdentity(b);
  if (a.commit !== b.commit || a.repository !== b.repository) throw new Error('mixed canonical sources');
}
export function canonicalSource(sources, fallback) {
  const canonical = sourceIdentity(sources[0] || fallback);
  for (const source of sources) sameSource(source, canonical);
  return canonical;
}
async function sidecar(release, name, key, fetcher) {
  const asset = release?.assets.find((a) => a.name === `${name}-${key}.identity.json`);
  if (!asset) return null;
  if (!/^sha256:[a-f0-9]{64}$/.test(asset.digest)) throw new Error('original identity asset lacks verified digest');
  const response = await request(asset.url, { ...githubHeaders(), Accept: 'application/octet-stream' }, fetcher);
  if (!response) throw new Error('original identity asset disappeared');
  const bytes = Buffer.from(await response.arrayBuffer());
  if (`sha256:${hash(bytes).toString('hex')}` !== asset.digest) throw new Error('identity asset digest mismatch');
  return JSON.parse(bytes);
}

export function manifests(env = process.env, root = '.') {
  const name = env.NAME;
  const dir = env.DIR || `packages/${name}`;
  const natives = env.NATIVES || 'packages/npm';
  const main = json(join(root, dir, 'package.json'));
  if (main.name !== env.PKG) throw new Error('launcher package name does not match npm-package');
  const platforms = readdirSync(join(root, natives)).sort();
  if (JSON.stringify(platforms) !== JSON.stringify([...KEYS].sort())) throw new Error('native packages must contain exactly the five release platforms');
  const packages = KEYS.map((key) => ({ key, dir: `${natives}/${key}`, ...json(join(root, natives, key, 'package.json')) }));
  if (packages.some((p) => main.optionalDependencies?.[p.name] !== main.version)) throw new Error('launcher native dependency version mismatch');
  packages.push({ dir, ...main });
  for (const alias of (env.ALIASES || '').split(/\s+/).filter(Boolean)) {
    const value = json(join(root, alias, 'package.json'));
    if (value.dependencies?.[main.name] !== main.version) throw new Error('alias launcher dependency version mismatch');
    packages.push({ dir: alias, ...value });
  }
  if (packages.some((p) => p.version !== main.version)) throw new Error('npm manifest version mismatch');
  const server = json(join(root, 'server.json'));
  if (server.name !== env.MCP_NAME || server.version !== main.version || !server.packages?.length || server.packages.some((p) => p.version !== main.version)) {
    throw new Error('server.json identity mismatch');
  }
  const launcher = server.packages.find((p) => p.registryType === 'npm' && p.identifier === main.name);
  if (!launcher) throw new Error('server.json does not name the npm launcher');
  return { name, version: main.version, dir, cargo: env.CARGO || name, packages };
}

export function cargoIdentity(metadata, cargo, version) {
  const selected = metadata.packages.filter((p) => p.name === cargo);
  if (selected.length !== 1 || selected[0].version !== version) throw new Error(`Cargo package ${cargo} must have version ${version}`);
}

export function requiredAssets(name, version, bundles, identities = true) {
  return [...KEYS.flatMap((key) => [archive(name, key), ...(identities ? [`${name}-${key}.identity.json`] : [])]), ...(bundles ? [`${name}-${version}.mcpb`, ...KEYS.map((key) => `${name}-${version}-${key}.mcpb`)] : [])];
}

export function completion(packages, release, mcp, docker, wantedAssets, wantsDocker) {
  const assets = new Set(release?.assets.map((a) => a.name) || []);
  const npmDone = packages.every(Boolean);
  const githubDone = Boolean(release) && wantedAssets.every((a) => assets.has(a));
  const registryDone = Boolean(mcp);
  const dockerDone = !wantsDocker || Boolean(docker);
  return { npm: npmDone, github: githubDone, registry: registryDone, docker: dockerDone,
    publish: !npmDone || !githubDone || !registryDone || !dockerDone };
}

export async function deliveryPlan(config, packages, release, mcp, { repository, commit, imageName = '', bundles = false }, fetcher = fetch) {
  const sources = [], nativeIdentities = {};
  for (const key of KEYS) {
    const identity = await sidecar(release, config.name, key, fetcher);
    if (identity) {
      if (identity.name !== config.name || identity.version !== config.version || identity.platform !== key || !/^[a-f0-9]{64}$/.test(identity.sha256)) throw new Error('original platform identity mismatch');
      sources.push(identity.source); nativeIdentities[key] = identity;
    }
  }
  for (const published of packages) {
    if (published) {
      if (!/^[a-f0-9]{40}$/.test(published.gitHead)) throw new Error('published package lacks canonical source');
      sources.push({ repository, commit: published.gitHead });
    }
  }
  const canonical = canonicalSource(sources, { repository, commit });
  let legacy = Boolean(release) && Object.keys(nativeIdentities).length === 0;
  const hydrate = async () => {
    for (const key of KEYS) {
      const published = packages[KEYS.indexOf(key)];
      if (!nativeIdentities[key] && published) {
        const pkg = config.packages?.find((p) => p.key === key) || published;
        const { identity } = await npmNative(config, key, pkg, published, canonical, fetcher);
        nativeIdentities[key] = identity;
      }
    }
    if (!Object.keys(nativeIdentities).length) throw new Error('legacy partial release lacks original identities; requested channel recovery requires verified provenance');
    legacy = false;
  };
  const beforeImage = completion(packages, release, mcp, null, requiredAssets(config.name, config.version, bundles, !legacy), false);
  // Missing GitHub sidecars do not establish legacy provenance: npm may already
  // carry original identities from an interrupted modern publication.
  if (beforeImage.publish && (legacy || packages.some(Boolean))) await hydrate();
  let docker = await image(imageName, config.version, fetcher, canonical, nativeIdentities, legacy);
  if (legacy && imageName && !docker) {
    await hydrate();
    docker = await image(imageName, config.version, fetcher, canonical, nativeIdentities);
  }
  const done = completion(packages, release, mcp, docker, requiredAssets(config.name, config.version, bundles, !legacy), Boolean(imageName));
  return { canonical, done, legacy };
}

export function verifyIntegrity(bytes, integrity) {
  const valid = integrity.split(/\s+/).some((entry) => {
    const [algorithm, expected] = entry.split('-');
    return ['sha256', 'sha384', 'sha512'].includes(algorithm) && hash(bytes, algorithm).toString('base64') === expected;
  });
  if (!valid) throw new Error('npm tarball integrity mismatch');
}

export function nativeIdentity(name, version, key, bytes, source) {
  sourceIdentity(source);
  if (!KEYS.includes(key) || !bytes.length) throw new Error('invalid native identity');
  return { name, version, platform: key, sha256: hash(bytes).toString('hex'), source };
}
export function verifyNative(identity, name, version, key, bytes, canonical = identity.source) {
  sameSource(identity.source, canonical);
  if (identity.name !== name || identity.version !== version || identity.platform !== key || !identity.source || identity.sha256 !== hash(bytes).toString('hex')) {
    throw new Error(`native identity mismatch for ${key}`);
  }
}
export function verifyVersion(output, name, version) {
  if (output.trim() !== `${name} ${version}`) throw new Error(`binary must report ${name} ${version}`);
}

async function npmNative(config, key, pkg, published, canonical, fetcher) {
  if (!published.dist?.tarball || !published.dist.integrity) throw new Error('legacy partial release: npm binary lacks original build identity');
  const temporary = mkdtempSync(join(tmpdir(), 'release-npm-'));
  try {
    let bytes, identity;
    const response = await request(published.dist.tarball, {}, fetcher);
    if (!response) throw new Error('npm recovery tarball disappeared');
    const data = Buffer.from(await response.arrayBuffer());
    verifyIntegrity(data, published.dist.integrity);
    const file = join(temporary, 'native.tgz');
    writeFileSync(file, data);
    const manifest = JSON.parse(execFileSync('tar', ['-xOzf', file, 'package/package.json'], { encoding: 'utf8' }));
    if (manifest.name !== pkg.name || manifest.version !== config.version) throw new Error('recovered npm manifest identity mismatch');
    bytes = execFileSync('tar', ['-xOzf', file, `package/${binary(config.name, key)}`], { maxBuffer: 512 * 1024 * 1024 });
    // Preserve original identity carried in the signed package, never label
    // downloaded bytes with a requested version or current checkout.
    try { identity = JSON.parse(execFileSync('tar', ['-xOzf', file, 'package/identity.json'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] })); }
    catch { throw new Error('legacy partial release: npm binary lacks original build identity; verified provenance migration required'); }
    if (published.gitHead !== identity.source?.commit) throw new Error('npm gitHead differs from original build source');
    verifyNative(identity, config.name, config.version, key, bytes, canonical);
    return { bytes, identity };
  } finally { rmSync(temporary, { recursive: true, force: true }); }
}

export async function recover(config, key, out, fetcher = fetch) {
  const pkg = config.packages.find((p) => p.key === key);
  const published = await npm(pkg.name, config.version, fetcher);
  const release = await github(process.env.GITHUB_REPOSITORY, config.version, fetcher);
  const original = await sidecar(release, config.name, key, fetcher);
  const asset = original && release?.assets.find((a) => a.name === archive(config.name, key) && /^sha256:[a-f0-9]{64}$/.test(a.digest));
  if (!published && !asset) {
    if (release?.assets.some((a) => a.name === archive(config.name, key))) throw new Error('legacy release binary lacks verified original identity; migration required');
    return false;
  }
  const temporary = mkdtempSync(join(tmpdir(), 'release-native-'));
  try {
    let bytes, identity;
    const canonical = sourceIdentity(config.source || { repository: process.env.GITHUB_REPOSITORY, commit: process.env.CANONICAL_SHA });
    if (asset) {
      const response = await request(asset.url, { ...githubHeaders(), Accept: 'application/octet-stream' }, fetcher);
      if (!response) throw new Error('GitHub recovery asset disappeared');
      const data = Buffer.from(await response.arrayBuffer());
      if (`sha256:${hash(data).toString('hex')}` !== asset.digest) throw new Error('GitHub recovery asset digest mismatch');
      const file = join(temporary, key.startsWith('win32') ? 'native.zip' : 'native.tar.gz');
      writeFileSync(file, data);
      bytes = key.startsWith('win32')
        ? execFileSync('unzip', ['-p', file, binary(config.name, key)], { maxBuffer: 512 * 1024 * 1024 })
        : execFileSync('tar', ['-xOzf', file, binary(config.name, key)], { maxBuffer: 512 * 1024 * 1024 });
      identity = original;
      if (!identity) throw new Error('legacy GitHub binary lacks original verified build identity; recover from proven npm package instead');
    } else {
      ({ bytes, identity } = await npmNative(config, key, pkg, published, canonical, fetcher));
    }
    verifyNative(identity, config.name, config.version, key, bytes, canonical);
    mkdirSync(out, { recursive: true });
    writeFileSync(join(out, binary(config.name, key)), bytes, { mode: 0o755 });
    writeFileSync(join(out, 'identity.json'), JSON.stringify(identity));
    return true;
  } finally { rmSync(temporary, { recursive: true, force: true }); }
}

async function wait(probe) {
  for (let attempt = 0; attempt < 120; attempt++) {
    if (await probe()) return;
    await new Promise((r) => setTimeout(r, 5000));
  }
  throw new Error('publication not visible after 10 minutes');
}
// Descriptor-driven version manifests (offline): `version set <X.Y.Z>` and `version check`.
// A descriptor lists every location that carries the product version; see README "Version manifests".
const SEMVER = /^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/;
const KINDS = ['json', 'toml', 'cargo-lock', 'regex'];

// A file pattern may use `*` inside one path segment (packages/npm/*/package.json).
export function expandFiles(root, pattern) {
  let paths = [''];
  for (const part of pattern.split('/')) {
    if (!part.includes('*')) { paths = paths.map((p) => (p ? `${p}/${part}` : part)); continue; }
    const re = new RegExp(`^${part.replace(/[.+?^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '[^/]*')}$`);
    paths = paths.flatMap((p) => {
      try { return readdirSync(join(root, p)).filter((n) => re.test(n)).sort().map((n) => (p ? `${p}/${n}` : n)); } catch { return []; }
    });
  }
  return paths;
}

const segments = (field) => (Array.isArray(field) ? field : String(field).split('.'));

function jsonFind(text, loc, file) {
  const data = JSON.parse(text);
  const indent = /^\n?([ \t]+)\S/m.exec(text)?.[1] ?? '  ';
  const entries = [];
  const walk = (node, rest, trail) => {
    if (!rest.length) { entries.push({ label: trail.join('.'), value: node, set: null }); return; }
    const [head, ...tail] = rest;
    if (node === null || typeof node !== 'object') return;
    for (const key of head === '*' ? Object.keys(node) : Object.hasOwn(node, head) ? [head] : []) {
      if (tail.length) walk(node[key], tail, [...trail, key]);
      else entries.push({ label: [...trail, key].join('.'), value: node[key], set: (v) => { node[key] = v; } });
    }
  };
  for (const field of loc.fields) {
    const before = entries.length;
    walk(data, segments(field), []);
    if (entries.length === before) throw new Error(`${file}: field ${segments(field).join('.')} not found`);
  }
  return { entries, render: () => `${JSON.stringify(data, null, indent)}${text.endsWith('\n') ? '\n' : ''}` };
}

const unquote = (k) => (k.startsWith('"') || k.startsWith("'") ? k.slice(1, -1) : k);
function tomlSpans(text) {
  // String values of `key = "x"` lines and of fields inside single-line inline tables, with their table path.
  const spans = [];
  let table = [];
  let offset = 0;
  for (const raw of text.split('\n')) {
    const line = raw.replace(/\r$/, '');
    const head = /^\s*\[(?!\[)([^\]]+)\]\s*(?:#.*)?$/.exec(line);
    if (head) table = head[1].split('.').map((s) => unquote(s.trim()));
    else if (/^\s*\[\[/.test(line)) table = ['[[array]]'];
    else {
      const key = /^(\s*)("[^"]+"|'[^']+'|[A-Za-z0-9_-]+)\s*=\s*/.exec(line);
      if (key) {
        const name = unquote(key[2]);
        const value = line.slice(key[0].length);
        const str = /^"((?:[^"\\]|\\.)*)"/.exec(value);
        const at = offset + key[0].length;
        if (str) spans.push({ path: [...table, name], start: at + 1, end: at + 1 + str[1].length, value: str[1] });
        else if (value.startsWith('{')) {
          for (const m of value.matchAll(/([{,]\s*)("[^"]+"|[A-Za-z0-9_-]+)\s*=\s*"((?:[^"\\]|\\.)*)"/g)) {
            const start = at + m.index + m[0].length - m[3].length - 1;
            spans.push({ path: [...table, name, unquote(m[2])], start, end: start + m[3].length, value: m[3] });
          }
        }
      }
    }
    offset += raw.length + 1;
  }
  return spans;
}

function splice(text, spans, v) {
  let out = text;
  for (const s of [...spans].sort((a, b) => b.start - a.start)) out = out.slice(0, s.start) + v + out.slice(s.end);
  return out;
}

function tomlFind(text, loc, file) {
  const all = tomlSpans(text);
  const spans = [];
  for (const field of loc.fields) {
    const want = segments(field).join('\0');
    const hits = all.filter((s) => s.path.join('\0') === want);
    if (!hits.length) throw new Error(`${file}: field ${segments(field).join('.')} not found`);
    spans.push(...hits.map((s) => ({ ...s, label: segments(field).join('.') })));
  }
  return { entries: spans.map((s) => ({ label: s.label, value: s.value })), render: (v) => splice(text, spans, v) };
}

function lockFind(text, loc, file) {
  // Only workspace members (no `source`) of the listed names: registry crates and forks stay untouched.
  const spans = [];
  const blocks = [...text.matchAll(/^\[\[package\]\]\r?\n(?:(?!\[\[)[^\n]*\n?)*/gm)];
  for (const name of loc.packages) {
    const before = spans.length;
    for (const b of blocks) {
      if (new RegExp(`^name = "${name.replace(/[.+?^${}()|[\]\\]/g, '\\$&')}"\\r?$`, 'm').exec(b[0]) === null || /^source = /m.test(b[0])) continue;
      const m = /^version = "([^"]*)"/m.exec(b[0]);
      if (m) spans.push({ label: `package ${name}`, start: b.index + m.index + 11, end: b.index + m.index + 11 + m[1].length, value: m[1] });
    }
    if (spans.length === before) throw new Error(`${file}: workspace package ${name} not found`);
  }
  return { entries: spans.map((s) => ({ label: s.label, value: s.value })), render: (v) => splice(text, spans, v) };
}

function regexFind(text, loc, file) {
  if (new RegExp(`${loc.pattern}|`).exec('').length !== 2) throw new Error(`${file}: pattern needs exactly one capture group`);
  const re = new RegExp(loc.pattern, 'gm');
  const spans = [...text.matchAll(re)].map((m) => {
    const start = m.index + m[0].lastIndexOf(m[1]);
    return { label: `/${loc.pattern}/`, start, end: start + m[1].length, value: m[1] };
  });
  if (!spans.length) throw new Error(`${file}: pattern /${loc.pattern}/ not found`);
  return { entries: spans.map((s) => ({ label: s.label, value: s.value })), render: (v) => splice(text, spans, v) };
}

const FINDERS = { json: jsonFind, toml: tomlFind, 'cargo-lock': lockFind, regex: regexFind };

export function readDescriptor(file) {
  const d = json(file);
  const sections = ['product', 'pins', 'independent'];
  if (!d.source || !sections.some((s) => d[s]?.length)) throw new Error(`${file}: needs "source" and at least one of product, pins`);
  for (const loc of [d.source, ...sections.flatMap((s) => d[s] ?? [])]) {
    if (!loc.file || !KINDS.includes(loc.kind)) throw new Error(`${file}: each location needs "file" and "kind" (${KINDS.join(', ')})`);
    if ((loc.kind === 'json' || loc.kind === 'toml') && !loc.fields?.length) throw new Error(`${file}: ${loc.file} needs "fields"`);
    if (loc.kind === 'cargo-lock' && !loc.packages?.length) throw new Error(`${file}: ${loc.file} needs "packages"`);
    if (loc.kind === 'regex' && !loc.pattern) throw new Error(`${file}: ${loc.file} needs "pattern"`);
  }
  return d;
}

// Every file a location lists, parsed once: [{ role, file, loc, found }].
function resolveLocations(descriptor, root) {
  const out = [];
  for (const role of ['product', 'pins', 'independent']) {
    for (const loc of descriptor[role] ?? []) {
      const files = expandFiles(root, loc.file);
      if (!files.length) throw new Error(`${loc.file}: no file matches (${role})`);
      for (const file of files) out.push({ role, file, loc, found: FINDERS[loc.kind](readFileSync(join(root, file), 'utf8'), loc, file) });
    }
  }
  return out;
}

function sourceVersion(descriptor, root) {
  const files = expandFiles(root, descriptor.source.file);
  if (files.length !== 1) throw new Error(`source ${descriptor.source.file} must match exactly one file`);
  const { entries } = FINDERS[descriptor.source.kind](readFileSync(join(root, files[0]), 'utf8'), descriptor.source, files[0]);
  const values = new Set(entries.map((e) => e.value));
  if (values.size !== 1) throw new Error(`source ${files[0]} must carry exactly one value`);
  return [...values][0];
}

// Fails (returns problems) when any listed location disagrees with the source, or an independent one is absent or off its `expect`.
export function versionCheck(descriptor, root = '.', want = sourceVersion(descriptor, root)) {
  const problems = [];
  for (const { role, file, loc, found } of resolveLocations(descriptor, root)) {
    for (const e of found.entries) {
      if (role === 'independent') {
        if (loc.expect !== undefined && e.value !== loc.expect) problems.push(`${file} ${e.label}: independent version is ${e.value}, expected ${loc.expect}`);
      } else if (e.value !== want) problems.push(`${file} ${e.label}: ${e.value}, want ${want}`);
    }
  }
  return { want, problems };
}

// Rewrites product and pin locations to `v`; independent locations are read before and after and must not move.
export function versionSet(descriptor, v, root = '.') {
  if (!SEMVER.test(v)) throw new Error('version must look like X.Y.Z or X.Y.Z-pre');
  const before = resolveLocations(descriptor, root);
  const guarded = new Map();
  for (const r of before) if (r.role === 'independent') guarded.set(r.file, r.found.entries.map((e) => `${e.label}=${e.value}`).join('\n'));
  const changed = [];
  const writes = new Map();
  for (const r of before) {
    if (r.role === 'independent') continue;
    const file = join(root, r.file);
    // Each location edits the freshest text of its file, so two locations in one file compose.
    const text = writes.get(r.file) ?? readFileSync(file, 'utf8');
    const found = FINDERS[r.loc.kind](text, r.loc, r.file);
    if (r.loc.kind === 'json') { for (const e of found.entries) e.set(v); writes.set(r.file, found.render()); } else writes.set(r.file, found.render(v));
  }
  for (const [path, text] of writes) {
    if (readFileSync(join(root, path), 'utf8') !== text) { writeFileSync(join(root, path), text); changed.push(path); }
  }
  for (const r of resolveLocations(descriptor, root)) {
    if (r.role === 'independent' && r.found.entries.map((e) => `${e.label}=${e.value}`).join('\n') !== guarded.get(r.file)) {
      throw new Error(`${r.file}: independent version changed; fix the descriptor so no product location overlaps it`);
    }
  }
  return changed;
}

function versionCommand(args) {
  const [sub, ...rest] = args;
  const flag = rest.indexOf('--descriptor');
  const file = flag >= 0 ? rest.splice(flag, 2)[1] : 'version-manifests.json';
  const descriptor = readDescriptor(file);
  if (sub === 'set') {
    if (!SEMVER.test(rest[0] ?? '')) throw new Error('usage: release.mjs version set <X.Y.Z> [--descriptor FILE]');
    const changed = versionSet(descriptor, rest[0]);
    console.log(changed.length ? `set ${rest[0]}; changed:\n  ${changed.join('\n  ')}` : `already at ${rest[0]}`);
  } else {
    const { want, problems } = versionCheck(descriptor, '.', rest[0]);
    if (problems.length) throw new Error(`version drift (want ${want}):\n  ${problems.join('\n  ')}`);
    console.log(`every listed location is at ${want}`);
  }
}

const output = (name, value) => {
  const text = `${name}=${value}\n`;
  if (process.env.GITHUB_OUTPUT) writeFileSync(process.env.GITHUB_OUTPUT, text, { flag: 'a' });
  else process.stdout.write(text);
};

async function main() {
  const [command, ...args] = process.argv.slice(2);
  if (command === 'version' && (args[0] === 'set' || args[0] === 'check')) return versionCommand(args);
  if (command === 'probe' || command === 'wait' || command === 'require') {
    const [kind, name, version] = args;
    const probes = { npm, registry, crate, github, image };
    if (!probes[kind]) throw new Error('unknown publication kind');
    const nativeIdentities = kind === 'image' ? Object.fromEntries(['linux-x64-gnu', 'linux-arm64-gnu'].map((key) => [key, json(join('artifacts', `native-${key}`, 'identity.json'))])) : {};
    const probe = () => probes[kind](name, version, fetch, { repository: process.env.GITHUB_REPOSITORY, commit: process.env.CANONICAL_SHA }, nativeIdentities);
    if (command === 'wait') await wait(probe);
    else {
      const value = await probe();
      if (command === 'require' && !value) throw new Error(`${kind} ${name}@${version} is absent`);
      console.log(Boolean(value));
    }
    return;
  }
  if (command === 'deprecated') {
    const value = await registry(args[0], 'latest');
    console.log(value?._meta?.['io.modelcontextprotocol.registry/official']?.status === 'deprecated');
    return;
  }
  const config = manifests();
  if (command === 'plan') {
    cargoIdentity(JSON.parse(execFileSync('cargo', ['metadata', '--locked', '--no-deps', '--format-version', '1'], { encoding: 'utf8' })), config.cargo, config.version);
    const packages = [];
    for (const pkg of config.packages) packages.push(await npm(pkg.name, config.version));
    const release = await github(process.env.GITHUB_REPOSITORY, config.version);
    const mcp = await registry(process.env.MCP_NAME, config.version);
    const { canonical, done } = await deliveryPlan(config, packages, release, mcp, { repository: process.env.GITHUB_REPOSITORY, commit: process.env.GITHUB_SHA, imageName: process.env.IMAGE || '', bundles: process.env.MCPB === 'true' });
    output('canonical', canonical.commit);
    output('version', config.version); output('dir', config.dir); output('cargo', config.cargo); output('publish', done.publish);
    for (const channel of ['npm', 'github', 'registry', 'docker']) output(`${channel}-missing`, !done[channel]);
    console.log(`release ${config.version}: ${JSON.stringify(done)}`);
  } else if (command === 'recover') {
    output('recovered', await recover(config, args[0], 'out'));
  } else if (command === 'cargo') {
    cargoIdentity(JSON.parse(execFileSync('cargo', ['metadata', '--locked', '--no-deps', '--format-version', '1'], { encoding: 'utf8' })), config.cargo, config.version);
    if (execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim() !== process.env.CANONICAL_SHA) throw new Error('checkout differs from canonical source');
  } else if (command === 'identity') {
    const key = args[0], file = join('out', binary(config.name, key));
    writeFileSync(join('out', 'identity.json'), JSON.stringify(nativeIdentity(config.name, config.version, key, readFileSync(file), sourceIdentity({ repository: process.env.GITHUB_REPOSITORY, commit: process.env.CANONICAL_SHA }))));
  } else if (command === 'version') {
    verifyVersion(execFileSync(args[0], ['version'], { encoding: 'utf8' }), config.name, config.version);
  } else if (command === 'stage') {
    for (const key of args.length ? args : KEYS) {
      const source = join('artifacts', `native-${key}`), bytes = readFileSync(join(source, binary(config.name, key)));
      const identity = json(join(source, 'identity.json'));
      verifyNative(identity, config.name, config.version, key, bytes, { repository: process.env.GITHUB_REPOSITORY, commit: process.env.CANONICAL_SHA });
      const destination = config.packages.find((p) => p.key === key).dir;
      copyFileSync(join(source, binary(config.name, key)), join(destination, binary(config.name, key)));
      chmodSync(join(destination, binary(config.name, key)), 0o755);
      copyFileSync(join(source, 'identity.json'), join(destination, 'identity.json'));
      const manifest = json(join(destination, 'package.json'));
      if (manifest.files && !manifest.files.includes('identity.json')) manifest.files.push('identity.json');
      writeFileSync(join(destination, 'package.json'), JSON.stringify(manifest, null, 2) + '\n');
    }
  } else if (command === 'assets') {
    const release = await github(process.env.GITHUB_REPOSITORY, config.version);
    for (const file of args) {
      const name = file.split('/').at(-1);
      if (!release?.assets.some((a) => a.name === name)) console.log(file);
    }
  } else throw new Error('unknown release command');
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  main().catch((error) => { console.error(error.message); process.exitCode = 1; });
}
