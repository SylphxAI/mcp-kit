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
const output = (name, value) => {
  const text = `${name}=${value}\n`;
  if (process.env.GITHUB_OUTPUT) writeFileSync(process.env.GITHUB_OUTPUT, text, { flag: 'a' });
  else process.stdout.write(text);
};

async function main() {
  const [command, ...args] = process.argv.slice(2);
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
