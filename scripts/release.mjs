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

export async function image(name, version, fetcher = fetch) {
  if (!name) return null;
  if (!name.startsWith('ghcr.io/')) throw new Error('docker-image must name a GHCR image');
  const repository = name.slice('ghcr.io/'.length);
  // GHCR's documented anonymous pull token, not a swallowed authentication failure.
  const tokenResponse = await request(`https://ghcr.io/token?service=ghcr.io&scope=${encode(`repository:${repository}:pull`)}`, {}, fetcher);
  if (!tokenResponse) throw new Error('GHCR token endpoint is absent');
  const { token } = await tokenResponse.json();
  if (!token) throw new Error('GHCR did not provide a pull token');
  const response = await request(`https://ghcr.io/v2/${repository}/manifests/${encode(version)}`, {
    Authorization: `Bearer ${token}`,
    Accept: 'application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json',
  }, fetcher);
  if (!response) return null;
  const bytes = Buffer.from(await response.arrayBuffer());
  const digest = `sha256:${hash(bytes).toString('hex')}`;
  if (response.headers.get('docker-content-digest') !== digest) throw new Error('GHCR manifest digest mismatch');
  const value = JSON.parse(bytes);
  for (const arch of ['amd64', 'arm64']) {
    if (!value.manifests?.some((m) => m.platform?.os === 'linux' && m.platform?.architecture === arch && /^sha256:[a-f0-9]{64}$/.test(m.digest))) {
      throw new Error(`GHCR image lacks linux/${arch}`);
    }
  }
  return { digest };
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

export function requiredAssets(name, version, bundles) {
  return [...KEYS.map((key) => archive(name, key)), ...(bundles ? [`${name}-${version}.mcpb`, ...KEYS.map((key) => `${name}-${version}-${key}.mcpb`)] : [])];
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

export function verifyIntegrity(bytes, integrity) {
  const valid = integrity.split(/\s+/).some((entry) => {
    const [algorithm, expected] = entry.split('-');
    return ['sha256', 'sha384', 'sha512'].includes(algorithm) && hash(bytes, algorithm).toString('base64') === expected;
  });
  if (!valid) throw new Error('npm tarball integrity mismatch');
}

export function nativeIdentity(name, version, key, bytes, source) {
  if (!KEYS.includes(key) || !bytes.length || !source) throw new Error('invalid native identity');
  return { name, version, platform: key, sha256: hash(bytes).toString('hex'), source };
}
export function verifyNative(identity, name, version, key, bytes) {
  if (identity.name !== name || identity.version !== version || identity.platform !== key || !identity.source || identity.sha256 !== hash(bytes).toString('hex')) {
    throw new Error(`native identity mismatch for ${key}`);
  }
}
export function verifyVersion(output, name, version) {
  if (output.trim() !== `${name} ${version}`) throw new Error(`binary must report ${name} ${version}`);
}

export async function recover(config, key, out, fetcher = fetch) {
  const pkg = config.packages.find((p) => p.key === key);
  const published = await npm(pkg.name, config.version, fetcher);
  const release = await github(process.env.GITHUB_REPOSITORY, config.version, fetcher);
  const asset = release?.assets.find((a) => a.name === archive(config.name, key) && /^sha256:[a-f0-9]{64}$/.test(a.digest));
  if (!published && !asset) return false;
  const temporary = mkdtempSync(join(tmpdir(), 'release-native-'));
  try {
    let bytes, source;
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
      source = { release: release.tag_name, digest: asset.digest };
    } else {
      const response = await request(published.dist.tarball, {}, fetcher);
      if (!response) throw new Error('npm recovery tarball disappeared');
      const data = Buffer.from(await response.arrayBuffer());
      verifyIntegrity(data, published.dist.integrity);
      const file = join(temporary, 'native.tgz');
      writeFileSync(file, data);
      const manifest = JSON.parse(execFileSync('tar', ['-xOzf', file, 'package/package.json'], { encoding: 'utf8' }));
      if (manifest.name !== pkg.name || manifest.version !== config.version) throw new Error('recovered npm manifest identity mismatch');
      bytes = execFileSync('tar', ['-xOzf', file, `package/${binary(config.name, key)}`], { maxBuffer: 512 * 1024 * 1024 });
      source = { npm: pkg.name, integrity: published.dist.integrity };
    }
    mkdirSync(out, { recursive: true });
    writeFileSync(join(out, binary(config.name, key)), bytes, { mode: 0o755 });
    writeFileSync(join(out, 'identity.json'), JSON.stringify(nativeIdentity(config.name, config.version, key, bytes, source)));
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
    const probe = () => probes[kind](name, version);
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
    const docker = await image(process.env.IMAGE || '', config.version);
    const done = completion(packages, release, mcp, docker, requiredAssets(config.name, config.version, process.env.MCPB === 'true'), Boolean(process.env.IMAGE));
    output('version', config.version); output('dir', config.dir); output('cargo', config.cargo); output('publish', done.publish);
    for (const channel of ['npm', 'github', 'registry', 'docker']) output(`${channel}-missing`, !done[channel]);
    console.log(`release ${config.version}: ${JSON.stringify(done)}`);
  } else if (command === 'recover') {
    output('recovered', await recover(config, args[0], 'out'));
  } else if (command === 'identity') {
    const key = args[0], file = join('out', binary(config.name, key));
    writeFileSync(join('out', 'identity.json'), JSON.stringify(nativeIdentity(config.name, config.version, key, readFileSync(file), { commit: process.env.GITHUB_SHA })));
  } else if (command === 'version') {
    verifyVersion(execFileSync(args[0], ['version'], { encoding: 'utf8' }), config.name, config.version);
  } else if (command === 'stage') {
    for (const key of args.length ? args : KEYS) {
      const source = join('artifacts', `native-${key}`), bytes = readFileSync(join(source, binary(config.name, key)));
      verifyNative(json(join(source, 'identity.json')), config.name, config.version, key, bytes);
      const destination = config.packages.find((p) => p.key === key).dir;
      copyFileSync(join(source, binary(config.name, key)), join(destination, binary(config.name, key)));
      chmodSync(join(destination, binary(config.name, key)), 0o755);
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
