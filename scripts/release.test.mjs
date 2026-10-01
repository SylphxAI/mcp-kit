// Offline fixtures only: no registry requests, credentials, models or Rust builds.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { KEYS, request, npm, registry, crate, github, image, manifests, cargoIdentity, requiredAssets, completion, verifyIntegrity, nativeIdentity, verifyNative, verifyVersion, recover } from './release.mjs';

const response = (status, value) => new Response(JSON.stringify(value), { status });
const fixture = (value) => async () => response(200, value);
const npmValue = { name: '@x/tool', version: '1.2.3', dist: { tarball: 'https://registry.npmjs.org/native.tgz', integrity: 'sha512-fixture' } };
const releaseValue = { tag_name: 'v1.2.3', draft: false, assets: [] };
const registryValue = { server: { name: 'io.github.x/tool', version: '1.2.3' }, _meta: { 'io.modelcontextprotocol.registry/official': { status: 'active' } } };

for (const status of [401, 403, 429, 500, 503]) {
  test(`HTTP ${status} cannot authorize publication or retirement`, async () => {
    for (const probe of [npm, registry, crate, github, image]) {
      await assert.rejects(probe(probe === image ? 'ghcr.io/x/tool' : 'x', '1.2.3', async () => response(status, {})), /release probe failed/);
    }
  });
}
test('only HTTP 404 is absent; transport and malformed responses fail', async () => {
  assert.equal(await request('https://example.test/x', {}, async () => response(404, {})), null);
  for (const probe of [npm, registry, crate, github]) {
    assert.equal(await probe('x', '1.2.3', async () => response(404, {})), null);
    await assert.rejects(probe('x', '1.2.3', async () => { throw new Error('transport failure'); }), /transport failure/);
    await assert.rejects(probe('x', '1.2.3', async () => new Response('not JSON')), SyntaxError);
  }
});
test('official publication identities are checked, not merely HTTP success', async () => {
  assert.equal((await npm('@x/tool', '1.2.3', fixture(npmValue))).version, '1.2.3');
  await assert.rejects(npm('@x/tool', '2.0.0', fixture(npmValue)), /identity mismatch/);
  await assert.rejects(npm('@x/other', '1.2.3', fixture(npmValue)), /identity mismatch/);
  assert.ok(await registry('io.github.x/tool', '1.2.3', fixture(registryValue)));
  await assert.rejects(registry('io.github.x/other', '1.2.3', fixture(registryValue)), /identity mismatch/);
  await assert.rejects(registry('io.github.x/tool', '2.0.0', fixture(registryValue)), /identity mismatch/);
  await assert.rejects(registry('io.github.x/tool', '1.2.3', fixture({ ...registryValue, _meta: { 'io.modelcontextprotocol.registry/official': { status: 'deprecated' } } })), /inactive version/);
  assert.ok(await github('x/tool', '1.2.3', fixture(releaseValue)));
  await assert.rejects(github('x/tool', '2.0.0', fixture(releaseValue)), /identity mismatch/);
  await assert.rejects(github('x/tool', '1.2.3', fixture({ ...releaseValue, draft: true })), /identity mismatch/);
  const value = { version: { crate: 'tool', num: '1.2.3', yanked: false } };
  assert.ok(await crate('tool', '1.2.3', fixture(value)));
  await assert.rejects(crate('other', '1.2.3', fixture(value)), /identity mismatch/);
  await assert.rejects(crate('tool', '1.2.3', fixture({ version: { ...value.version, yanked: true } })), /yanked/);
});
test('all requested channels, all five natives and aliases determine completion', () => {
  const assets = requiredAssets('tool', '1.2.3', true);
  assert.equal(assets.length, 11);
  const release = { assets: assets.map((name) => ({ name })) };
  const packages = [...KEYS.map(() => true), true, true, true];
  const done = (p = packages, r = release, m = registryValue, d = {}) => completion(p, r, m, d, assets, true);
  assert.equal(done().publish, false);
  for (let i = 0; i < packages.length; i++) {
    const partial = [...packages]; partial[i] = false;
    assert.equal(done(partial).publish, true, `missing native/launcher/alias ${i}`);
  }
  assert.equal(done(packages, null).publish, true);
  for (let i = 0; i < assets.length; i++) {
    assert.equal(done(packages, { assets: release.assets.filter((_, index) => index !== i) }).github, false);
  }
  assert.equal(done(packages, release, null).publish, true, 'npm + GitHub is not registry completion');
  assert.equal(done(packages, release, registryValue, null).publish, true, 'requested image must recover');
  assert.equal(completion(packages, release, registryValue, null, assets, false).publish, false, 'unrequested Docker is optional');
});
test('Cargo metadata and runnable binary versions must match the launcher', () => {
  cargoIdentity({ packages: [{ name: 'tool', version: '1.2.3' }] }, 'tool', '1.2.3');
  assert.throws(() => cargoIdentity({ packages: [{ name: 'other', version: '1.2.3' }] }, 'tool', '1.2.3'));
  assert.throws(() => cargoIdentity({ packages: [{ name: 'tool', version: '1.2.2' }] }, 'tool', '1.2.3'));
  verifyVersion('tool 1.2.3\n', 'tool', '1.2.3');
  for (const value of ['tool 1.2.2', 'other 1.2.3', 'tool 1.2.3\nextra']) assert.throws(() => verifyVersion(value, 'tool', '1.2.3'));
});
test('cross-compiled identity remains bound to exact platform, version and bytes', () => {
  const bytes = Buffer.from('fixture binary');
  for (const key of KEYS) {
    const identity = nativeIdentity('tool', '1.2.3', key, bytes, { commit: 'fixture' });
    verifyNative(identity, 'tool', '1.2.3', key, bytes);
    for (const override of [{ name: 'other' }, { version: '1.2.2' }, { platform: 'wrong' }, { source: null }, { sha256: 'wrong' }]) {
      assert.throws(() => verifyNative({ ...identity, ...override }, 'tool', '1.2.3', key, bytes));
    }
    assert.throws(() => verifyNative(identity, 'tool', '1.2.3', key, Buffer.from('different binary')));
  }
});
test('manifest fixtures keep all platforms, aliases and MCP launcher identity', () => {
  const root = mkdtempSync(join(tmpdir(), 'release-manifests-'));
  const write = (path, value) => { mkdirSync(join(root, path), { recursive: true }); writeFileSync(join(root, path, 'package.json'), JSON.stringify(value)); };
  const env = { NAME: 'tool', PKG: '@x/tool', MCP_NAME: 'io.github.x/tool', ALIASES: 'packages/alias' };
  try {
    for (const key of KEYS) write(`packages/npm/${key}`, { name: `@x/tool-${key}`, version: '1.2.3' });
    write('packages/tool', { name: '@x/tool', version: '1.2.3', optionalDependencies: Object.fromEntries(KEYS.map((key) => [`@x/tool-${key}`, '1.2.3'])) });
    write('packages/alias', { name: '@x/alias', version: '1.2.3', dependencies: { '@x/tool': '1.2.3' } });
    writeFileSync(join(root, 'server.json'), JSON.stringify({ ...registryValue.server, packages: [{ registryType: 'npm', identifier: '@x/tool', version: '1.2.3' }] }));
    assert.equal(manifests(env, root).packages.length, 7);
    write('packages/alias', { name: '@x/alias', version: '1.2.2', dependencies: { '@x/tool': '1.2.3' } });
    assert.throws(() => manifests(env, root), /version mismatch/);
    write('packages/alias', { name: '@x/alias', version: '1.2.3', dependencies: { '@x/tool': '1.2.2' } });
    assert.throws(() => manifests(env, root), /dependency version mismatch/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
test('npm tarball integrity fails closed', () => {
  const bytes = Buffer.from('fixture tarball');
  const integrity = `sha512-${createHash('sha512').update(bytes).digest('base64')}`;
  verifyIntegrity(bytes, integrity);
  assert.throws(() => verifyIntegrity(Buffer.from('changed'), integrity), /integrity mismatch/);
  assert.throws(() => verifyIntegrity(bytes, 'sha1-legacy'), /integrity mismatch/);
});
test('fresh recovery uses exact npm artifacts when old GitHub assets lack digests', async () => {
  const root = mkdtempSync(join(tmpdir(), 'release-recovery-'));
  try {
    mkdirSync(join(root, 'package'));
    const bytes = Buffer.from('tool fixture');
    writeFileSync(join(root, 'package/tool'), bytes);
    writeFileSync(join(root, 'package/package.json'), JSON.stringify({ name: '@x/tool-native', version: '1.2.3' }));
    execFileSync('tar', ['-czf', join(root, 'native.tgz'), '-C', root, 'package']);
    const tarball = readFileSync(join(root, 'native.tgz'));
    const value = { ...npmValue, name: '@x/tool-native', dist: { tarball: 'https://registry.npmjs.org/native.tgz', integrity: `sha512-${createHash('sha512').update(tarball).digest('base64')}` } };
    const config = { name: 'tool', version: '1.2.3', packages: [{ key: 'linux-arm64-gnu', name: value.name }] };
    const fetcher = async (url) => {
      if (url === value.dist.tarball) return new Response(tarball);
      if (url.startsWith('https://api.github.com/')) return response(200, { ...releaseValue, assets: [{ name: 'tool-linux-arm64-gnu.tar.gz' }] });
      return response(200, value);
    };
    const out = join(root, 'out');
    assert.equal(await recover(config, 'linux-arm64-gnu', out, fetcher), true);
    assert.deepEqual(readFileSync(join(out, 'tool')), bytes);
    verifyNative(JSON.parse(readFileSync(join(out, 'identity.json'))), 'tool', '1.2.3', 'linux-arm64-gnu', bytes);
    await assert.rejects(recover(config, 'linux-arm64-gnu', out, async (url) => url === value.dist.tarball ? new Response('wrong bytes') : fetcher(url)), /integrity mismatch/);
    execFileSync('tar', ['-czf', join(root, 'github.tar.gz'), '-C', join(root, 'package'), 'tool']);
    const githubBytes = readFileSync(join(root, 'github.tar.gz'));
    const asset = { name: 'tool-linux-arm64-gnu.tar.gz', url: 'https://api.github.com/asset', digest: `sha256:${createHash('sha256').update(githubBytes).digest('hex')}` };
    const fromGitHub = async (url) => {
      if (url === asset.url) return new Response(githubBytes);
      if (url.startsWith('https://api.github.com/')) return response(200, { ...releaseValue, assets: [asset] });
      return response(404, {});
    };
    assert.equal(await recover(config, 'linux-arm64-gnu', out, fromGitHub), true);
    assert.equal(JSON.parse(readFileSync(join(out, 'identity.json'))).source.digest, asset.digest);
    await assert.rejects(recover(config, 'linux-arm64-gnu', out, async (url) => url === asset.url ? new Response('tampered archive') : fromGitHub(url)), /digest mismatch/);
    assert.equal(await recover(config, 'linux-arm64-gnu', out, async () => response(404, {})), false, 'only confirmed absence permits a new build');
    await assert.rejects(recover(config, 'linux-arm64-gnu', out, async () => response(503, {})), /release probe failed/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
test('GHCR readback binds the version tag to digest and both Linux architectures', async () => {
  const manifests = ['amd64', 'arm64'].map((architecture) => ({ digest: `sha256:${'a'.repeat(64)}`, platform: { os: 'linux', architecture } }));
  const bytes = JSON.stringify({ manifests });
  const digest = `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
  const fetcher = async (url) => url.startsWith('https://ghcr.io/token?') ? response(200, { token: 'offline-fixture' }) : new Response(bytes, { headers: { 'docker-content-digest': digest } });
  assert.deepEqual(await image('ghcr.io/x/tool', '1.2.3', fetcher), { digest });
  await assert.rejects(image('ghcr.io/x/tool', '1.2.3', async (url) => url.startsWith('https://ghcr.io/token?') ? response(200, { token: 'offline-fixture' }) : new Response(bytes)), /digest mismatch/);
  assert.equal(await image('ghcr.io/x/tool', '1.2.3', async (url) => url.startsWith('https://ghcr.io/token?') ? response(200, { token: 'offline-fixture' }) : response(404, {})), null);
});
test('workflow keeps required build success and verifies replacement before retirement', () => {
  const workflow = readFileSync(new URL('../.github/workflows/release.yml', import.meta.url), 'utf8');
  assert.match(workflow, /publish:\n    needs: \[check, build\]/);
  assert.match(workflow, /docker:\n    needs: \[check, build\]/);
  assert.match(workflow, /registry-retired:\n    needs: \[check, build, publish\]/);
  assert.match(workflow, /needs\.check\.outputs\.publish == 'false' && needs\.build\.result == 'skipped'/);
  const retirement = workflow.slice(workflow.indexOf('  registry-retired:'));
  assert.ok(retirement.indexOf('require registry "$MCP_NAME" "$V"') < retirement.indexOf('for retired in $RETIRED'));
  assert.match(workflow, /run: node \.mcp-kit\/scripts\/release\.mjs version/);
  assert.match(workflow, /run: node \.mcp-kit\/scripts\/release\.mjs recover/);
  assert.match(workflow, /stage linux-x64-gnu linux-arm64-gnu/);
  assert.doesNotMatch(workflow, /if ! npm view|if gh release view|find "artifacts/);
});
