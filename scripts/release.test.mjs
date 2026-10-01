// Offline fixtures only: no registry requests, credentials, models or Rust builds.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { KEYS, request, npm, registry, crate, github, image, manifests, cargoIdentity, requiredAssets, completion, verifyIntegrity, nativeIdentity, verifyNative, verifyVersion, recover, canonicalSource, sameSource, deliveryPlan, versionCheck, versionSet, readDescriptor, expandFiles } from './release.mjs';

const response = (status, value) => new Response(JSON.stringify(value), { status });
const fixture = (value) => async () => response(200, value);
const canonical = { repository: 'x/tool', commit: 'a'.repeat(40) };
const npmValue = { name: '@x/tool', version: '1.2.3', dist: { tarball: 'https://registry.npmjs.org/native.tgz', integrity: 'sha512-fixture' } };
const releaseValue = { tag_name: 'v1.2.3', draft: false, assets: [] };
const registryValue = { server: { name: 'io.github.x/tool', version: '1.2.3' }, _meta: { 'io.modelcontextprotocol.registry/official': { status: 'active' } } };

for (const status of [401, 403, 429, 500, 503]) {
  test(`HTTP ${status} cannot authorize publication or retirement`, async () => {
    for (const probe of [npm, registry, crate, github, image]) {
      await assert.rejects(probe(probe === image ? 'ghcr.io/x/tool' : 'x', '1.2.3', async () => response(status, {}), canonical), /release probe failed/);
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
  assert.equal(assets.length, 16);
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
    const identity = nativeIdentity('tool', '1.2.3', key, bytes, canonical);
    verifyNative(identity, 'tool', '1.2.3', key, bytes);
    for (const override of [{ name: 'other' }, { version: '1.2.2' }, { platform: 'wrong' }, { source: null }, { source: {} }, { source: { ...canonical, commit: 'b'.repeat(40) } }, { sha256: 'wrong' }]) {
      assert.throws(() => verifyNative({ ...identity, ...override }, 'tool', '1.2.3', key, bytes, canonical));
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
    writeFileSync(join(root, 'package/identity.json'), JSON.stringify(nativeIdentity('tool', '1.2.3', 'linux-arm64-gnu', bytes, canonical)));
    writeFileSync(join(root, 'package/package.json'), JSON.stringify({ name: '@x/tool-native', version: '1.2.3' }));
    execFileSync('tar', ['-czf', join(root, 'native.tgz'), '-C', root, 'package']);
    const tarball = readFileSync(join(root, 'native.tgz'));
    const value = { ...npmValue, gitHead: canonical.commit, name: '@x/tool-native', dist: { tarball: 'https://registry.npmjs.org/native.tgz', integrity: `sha512-${createHash('sha512').update(tarball).digest('base64')}` } };
    const config = { source: canonical, name: 'tool', version: '1.2.3', packages: [{ key: 'linux-arm64-gnu', name: value.name }] };
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
    const originalIdentity = Buffer.from(JSON.stringify(nativeIdentity('tool', '1.2.3', 'linux-arm64-gnu', bytes, canonical)));
    const identityAsset = { name: 'tool-linux-arm64-gnu.identity.json', url: 'https://api.github.com/identity', digest: `sha256:${createHash('sha256').update(originalIdentity).digest('hex')}` };
    const asset = { name: 'tool-linux-arm64-gnu.tar.gz', url: 'https://api.github.com/asset', digest: `sha256:${createHash('sha256').update(githubBytes).digest('hex')}` };
    const fromGitHub = async (url) => {
      if (url === identityAsset.url) return new Response(originalIdentity);
      if (url === asset.url) return new Response(githubBytes);
      if (url.startsWith('https://api.github.com/')) return response(200, { ...releaseValue, assets: [asset, identityAsset] });
      return response(404, {});
    };
    assert.equal(await recover(config, 'linux-arm64-gnu', out, fromGitHub), true);
    assert.equal(JSON.parse(readFileSync(join(out, 'identity.json'))).source.commit, canonical.commit);
    await assert.rejects(recover(config, 'linux-arm64-gnu', out, async (url) => url === asset.url ? new Response('tampered archive') : fromGitHub(url)), /digest mismatch/);
    assert.equal(await recover(config, 'linux-arm64-gnu', out, async () => response(404, {})), false, 'only confirmed absence permits a new build');
    await assert.rejects(recover(config, 'linux-arm64-gnu', out, async () => response(503, {})), /release probe failed/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
test('GHCR validates digest-addressed configs against canonical native identities', async () => {
  const natives = Object.fromEntries(['linux-x64-gnu', 'linux-arm64-gnu'].map((key) => [key, { sha256: 'f'.repeat(64) }]));
  const fixture = (stale = false) => {
    const documents = new Map();
    const store = (value) => { const bytes = JSON.stringify(value); const digest = `sha256:${createHash('sha256').update(bytes).digest('hex')}`; documents.set(digest, bytes); return digest; };
    const descriptors = ['amd64', 'arm64'].map((architecture) => {
      const key = architecture === 'amd64' ? 'linux-x64-gnu' : 'linux-arm64-gnu';
      const config = store({ architecture, os: 'linux', config: { Labels: {
        'org.opencontainers.image.version': stale ? '1.2.2' : '1.2.3',
        'org.opencontainers.image.revision': canonical.commit,
        'org.opencontainers.image.source': 'https://github.com/x/tool',
        [`io.sylphx.native.${key}.sha256`]: natives[key].sha256,
      } } });
      return { digest: store({ config: { digest: config } }), platform: { os: 'linux', architecture } };
    });
    const index = store({ manifests: descriptors });
    return async (url) => {
      if (url.startsWith('https://api.github.com/users/')) return response(200, { type: 'Organization' });
      if (url.startsWith('https://api.github.com/')) return response(200, { name: 'tool' });
      if (url.startsWith('https://ghcr.io/token?')) return response(200, { token: 'offline-fixture' });
      const digest = url.endsWith('/1.2.3') ? index : url.split('/').at(-1);
      return new Response(documents.get(digest), { headers: { 'docker-content-digest': digest } });
    };
  };
  assert.ok(await image('ghcr.io/x/tool', '1.2.3', fixture(), canonical, natives));
  await assert.rejects(image('ghcr.io/x/tool', '1.2.3', fixture(true), canonical, natives), /identity mismatch/);
  assert.equal(await image('ghcr.io/x/tool', '1.2.3', async (url) => url.includes('/users/') ? response(200, { type: 'Organization' }) : response(404, {}), canonical, natives), null);
  await assert.rejects(image('ghcr.io/x/tool', '1.2.3', async (url) => url.includes('/users/') ? response(200, { type: 'Organization' }) : url.startsWith('https://api.github.com/') ? response(200, {}) : response(403, {}), canonical, natives), /HTTP 403/);
});
test('canonical source rejects empty, invalid and mixed revisions', () => {
  assert.deepEqual(canonicalSource([canonical], canonical), canonical);
  assert.throws(() => canonicalSource([canonical, { ...canonical, commit: 'b'.repeat(40) }], canonical), /mixed/);
  assert.throws(() => canonicalSource([{}], canonical), /invalid/);
  assert.throws(() => sameSource(canonical, { ...canonical, repository: 'other/tool' }), /mixed/);
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

function legacyImageFixture(version, { stale = false, record = true, missing = false } = {}) {
  const documents = new Map();
  const store = (value) => {
    const bytes = JSON.stringify(value);
    const digest = `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
    documents.set(digest, bytes); return digest;
  };
  const manifests = ['amd64', 'arm64'].map((architecture) => ({
    digest: store({ config: { digest: store({ architecture, os: 'linux', config: stale ? { Labels: { 'org.opencontainers.image.version': '0.0.0' } } : {} }) } }),
    platform: { os: 'linux', architecture },
  }));
  const index = store({ manifests });
  return async (url) => {
    if (url.includes('/users/')) return response(200, { type: 'Organization' });
    if (url.includes('/versions?')) return response(200, [{ name: record ? index : `sha256:${'0'.repeat(64)}`, metadata: { container: { tags: [version] } } }]);
    if (url.startsWith('https://api.github.com/')) return response(missing ? 404 : 200, {});
    if (url.startsWith('https://ghcr.io/token?')) return response(200, { token: 'fixture' });
    const digest = url.endsWith(`/${version}`) ? index : url.split('/').at(-1);
    return new Response(documents.get(digest), { headers: { 'docker-content-digest': digest } });
  };
}
test('legacy lockdocs 0.4.0 and repomap 1.5.0 preserve all-channel complete no-op', async () => {
  for (const [name, version, aliases] of [['lockdocs', '0.4.0', []], ['repomap', '1.5.0', ['spine', 'locus', 'coderag']]]) {
    const config = { name, version };
    const packages = [...KEYS.map((key) => `${name}-${key}`), name, ...aliases].map((pkg) => ({ name: `@sylphx/${pkg}`, version, gitHead: canonical.commit }));
    const release = { tag_name: `v${version}`, assets: requiredAssets(name, version, true, false).map((name) => ({ name })) };
    assert.equal(release.assets.length, 11, 'five archives and six bundles, no new sidecars');
    const mcp = await registry(`io.github.SylphxAI/${name}`, version, fixture({ ...registryValue, server: { name: `io.github.SylphxAI/${name}`, version } }));
    const options = { repository: `SylphxAI/${name}`, commit: 'b'.repeat(40), bundles: true, imageName: name === 'repomap' ? 'ghcr.io/sylphxai/repomap' : '' };
    const fetcher = legacyImageFixture(version);
    const result = await deliveryPlan(config, packages, release, mcp, options, fetcher);
    assert.equal(result.legacy, true);
    assert.equal(result.done.publish, false);
    assert.equal(result.canonical.commit, canonical.commit);
    for (let i = 0; i < packages.length; i++) {
      const partial = [...packages]; partial[i] = null;
      await assert.rejects(deliveryPlan(config, partial, release, mcp, options, fetcher), /legacy partial/);
      const mixed = [...packages]; mixed[i] = { ...mixed[i], gitHead: 'c'.repeat(40) };
      await assert.rejects(deliveryPlan(config, mixed, release, mcp, options, fetcher), /mixed canonical/);
    }
    for (let i = 0; i < release.assets.length; i++) await assert.rejects(deliveryPlan(config, packages, { ...release, assets: release.assets.filter((_, j) => i !== j) }, mcp, options, fetcher), /legacy partial/);
    await assert.rejects(deliveryPlan(config, packages, release, null, options, fetcher), /legacy partial/);
    if (options.imageName) {
      await assert.rejects(deliveryPlan(config, packages, release, mcp, options, legacyImageFixture(version, { missing: true })), /legacy partial/);
      await assert.rejects(deliveryPlan(config, packages, release, mcp, options, legacyImageFixture(version, { record: false })), /version record mismatch/);
      await assert.rejects(deliveryPlan(config, packages, release, mcp, options, legacyImageFixture(version, { stale: true })), /identity mismatch/);
      const imageRecord = await image(options.imageName, version, fetcher, result.canonical, {}, true);
      assert.equal(imageRecord.verification, 'existing-version-record', 'does not claim native provenance');
    }
  }
});
test('later same-version main B recovers four platforms and compiles missing platform from A', async () => {
  const missingKey = KEYS.at(-1), version = '8.3.0', name = 'anymd';
  const packages = KEYS.map((key) => key === missingKey ? null : { gitHead: canonical.commit });
  packages.push({ gitHead: canonical.commit });
  const identities = KEYS.filter((key) => key !== missingKey).map((key) => {
    const bytes = Buffer.from(JSON.stringify(nativeIdentity(name, version, key, Buffer.from(key), canonical)));
    return { name: `${name}-${key}.identity.json`, digest: `sha256:${createHash('sha256').update(bytes).digest('hex')}`, url: `https://fixture.test/${key}`, bytes };
  });
  const result = await deliveryPlan({ name, version }, packages, { assets: identities }, registryValue,
    { repository: canonical.repository, commit: 'b'.repeat(40) }, async (url) => new Response(identities.find((asset) => asset.url === url).bytes));
  assert.equal(result.canonical.commit, canonical.commit);
  assert.equal(result.done.publish, true);
  const workflow = readFileSync(new URL('../.github/workflows/release.yml', import.meta.url), 'utf8');
  for (const job of ['build', 'publish', 'docker']) {
    const section = workflow.slice(workflow.indexOf(`\n  ${job}:\n`) + 1).split(/\n  [a-z][a-z-]*:/)[0];
    assert.match(section, /- uses: actions\/checkout@v4\n        with:\n          ref: \$\{\{ needs.check.outputs.canonical \}\}/);
    assert.match(section, /repository: SylphxAI\/mcp-kit\n          ref: \$\{\{ inputs.kit-ref \}\}/);
  }
  const newIdentity = nativeIdentity(name, version, missingKey, Buffer.from('new binary'), result.canonical);
  assert.equal(newIdentity.source.commit, canonical.commit);
});
test('fully absent anymd 8.3.0 remains a fresh canonical identity-backed release', async () => {
  const trigger = { repository: 'SylphxAI/anymd', commit: 'b'.repeat(40) };
  const result = await deliveryPlan({ name: 'anymd', version: '8.3.0' }, Array(6).fill(null), null, null,
    { ...trigger, bundles: true, imageName: 'ghcr.io/sylphxai/anymd' }, async (url) => url.includes('/users/') ? response(200, { type: 'Organization' }) : response(404, {}));
  assert.equal(result.legacy, false);
  assert.deepEqual(result.canonical, trigger);
  assert.deepEqual(result.done, { npm: false, github: false, registry: false, docker: false, publish: true });
  for (const key of KEYS) assert.deepEqual(nativeIdentity('anymd', '8.3.0', key, Buffer.from(key), result.canonical).source, trigger);
});

test('interrupted modern npm delivery before GitHub sidecars resumes using original embedded identities', async () => {
  const root = mkdtempSync(join(tmpdir(), 'release-interrupted-'));
  const name = 'anymd', version = '8.3.0';
  const tarballs = new Map();
  const makePackage = (key, source = canonical, withIdentity = true) => {
    const path = join(root, key);
    mkdirSync(join(path, 'package'), { recursive: true });
    const pkg = { key, name: `@sylphx/${name}-${key}`, version, gitHead: canonical.commit };
    const bytes = Buffer.from(`fixture ${key}`);
    writeFileSync(join(path, 'package', key.startsWith('win32') ? 'anymd.exe' : 'anymd'), bytes);
    writeFileSync(join(path, 'package/package.json'), JSON.stringify(pkg));
    if (withIdentity) writeFileSync(join(path, 'package/identity.json'), JSON.stringify(nativeIdentity(name, version, key, bytes, source)));
    execFileSync('tar', ['-czf', join(path, 'native.tgz'), '-C', path, 'package']);
    const tarball = readFileSync(join(path, 'native.tgz'));
    const url = `https://fixture.test/${key}.tgz`;
    tarballs.set(url, tarball);
    return { ...pkg, dist: { tarball: url, integrity: `sha512-${createHash('sha512').update(tarball).digest('base64')}` } };
  };
  try {
    const packages = KEYS.map((key) => makePackage(key));
    packages.push({ name: '@sylphx/anymd', version, gitHead: canonical.commit });
    const config = { name, version, packages };
    const options = { repository: canonical.repository, commit: 'b'.repeat(40), bundles: true };
    const release = { ...releaseValue, tag_name: `v${version}`, assets: [{ name: `${name}-${version}.mcpb` }] };
    const fetcher = async (url) => {
      assert.ok(tarballs.has(url), 'planner only reads immutable npm tarballs');
      return new Response(tarballs.get(url));
    };
    const result = await deliveryPlan(config, packages, release, null, options, fetcher);
    assert.equal(result.legacy, false);
    assert.equal(result.canonical.commit, canonical.commit);
    assert.deepEqual(result.done, { npm: true, github: false, registry: false, docker: true, publish: true });
    assert.deepEqual(release.assets, [{ name: `${name}-${version}.mcpb` }], 'existing assets unchanged');
    // Reuse the same loader during actual native recovery; do not regenerate identity.
    const native = packages[0], out = join(root, 'out');
    await recover({ ...config, source: result.canonical }, native.key, out, async (url) => {
      if (tarballs.has(url)) return fetcher(url);
      if (url.startsWith('https://api.github.com/')) return response(200, release);
      return response(200, native);
    });
    assert.deepEqual(JSON.parse(readFileSync(join(out, 'identity.json'))).source, canonical);
    const disagreement = [...packages];
    disagreement[0] = makePackage(KEYS[0], { ...canonical, commit: 'c'.repeat(40) });
    await assert.rejects(deliveryPlan(config, disagreement, release, null, options, fetcher), /gitHead differs/);
    disagreement[0] = makePackage(KEYS[0], { ...canonical, repository: 'other/anymd' });
    await assert.rejects(deliveryPlan(config, disagreement, release, null, options, fetcher), /mixed canonical/);
    // Clear a previously written sidecar to represent an actual pre-identity tarball.
    rmSync(join(root, KEYS[0], 'package/identity.json'));
    disagreement[0] = makePackage(KEYS[0], canonical, false);
    await assert.rejects(deliveryPlan(config, disagreement, release, null, options, fetcher), /legacy partial.*lacks original/);
    tarballs.set(packages[1].dist.tarball, Buffer.from('tampered'));
    await assert.rejects(deliveryPlan(config, packages, release, null, options, fetcher), /integrity mismatch/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

// Version manifests: one fixture tree in the anymd shape (workspace + lock, npm platform packages, aliases, server.json, CITATION.cff, independent forks).
const descriptor = {
  source: { file: 'packages/anymd/package.json', kind: 'json', fields: ['version'] },
  product: [
    { file: 'packages/anymd/package.json', kind: 'json', fields: ['version'] },
    { file: 'packages/npm/*/package.json', kind: 'json', fields: ['version'] },
    { file: 'packages/aliases/*/package.json', kind: 'json', fields: ['version'] },
    { file: 'server.json', kind: 'json', fields: ['version', 'packages.*.version'] },
    { file: 'Cargo.toml', kind: 'toml', fields: ['workspace.package.version'] },
    { file: 'Cargo.lock', kind: 'cargo-lock', packages: ['anymd', 'anymd-core'] },
    { file: 'CITATION.cff', kind: 'regex', pattern: '^version: (\\S+)$' },
  ],
  pins: [
    { file: 'packages/anymd/package.json', kind: 'json', fields: ['optionalDependencies.*'] },
    { file: 'packages/aliases/*/package.json', kind: 'json', fields: [['dependencies', '@sylphx/anymd']] },
    { file: 'Cargo.toml', kind: 'toml', fields: ['workspace.dependencies.anymd.version', 'workspace.dependencies.anymd-core.version'] },
  ],
  independent: [
    { file: 'Cargo.toml', kind: 'toml', fields: ['workspace.dependencies.anymd-pdf-extract.version'], expect: '0.12.2' },
    { file: 'Cargo.lock', kind: 'cargo-lock', packages: ['anymd-oar-ocr-vl'], expect: undefined },
  ],
};
const CARGO = `# workspace root
[workspace.package]
version = "1.0.0" # product version
edition = "2021"

[workspace.dependencies]
anymd = { path = "crates/anymd", version = "1.0.0" }
anymd-core = { path = "crates/core", version = "1.0.0" } # pin
anymd-pdf-extract = { path = "forks/pdf", version = "0.12.2" }
serde = { version = "1.0.0" }
`;
const LOCK = `# generated
version = 4

[[package]]
name = "anymd"
version = "1.0.0"
dependencies = ["anymd-core"]

[[package]]
name = "anymd-core"
version = "1.0.0"

[[package]]
name = "anymd-oar-ocr-vl"
version = "0.9.2"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
`;
const pkg = (o) => `${JSON.stringify(o, null, 2)}\n`;
function versionTree(over = {}) {
  const root = mkdtempSync(join(tmpdir(), 'kit-version-'));
  const put = (path, text) => { mkdirSync(join(root, path, '..'), { recursive: true }); writeFileSync(join(root, path), text); };
  put('packages/anymd/package.json', pkg({ name: '@sylphx/anymd', version: '1.0.0', optionalDependencies: { '@sylphx/anymd-linux-x64-gnu': '1.0.0', '@sylphx/anymd-darwin-arm64': '1.0.0' } }));
  put('packages/npm/linux/package.json', pkg({ name: '@sylphx/anymd-linux-x64-gnu', version: '1.0.0' }));
  put('packages/npm/darwin/package.json', pkg({ name: '@sylphx/anymd-darwin-arm64', version: '1.0.0' }));
  put('packages/aliases/alias/package.json', pkg({ name: 'anymd-cli', version: '1.0.0', dependencies: { '@sylphx/anymd': '1.0.0', other: '9.9.9' } }));
  put('server.json', pkg({ name: 'x', version: '1.0.0', packages: [{ version: '1.0.0' }] }));
  put('Cargo.toml', over.cargo ?? CARGO);
  put('Cargo.lock', over.lock ?? LOCK);
  put('CITATION.cff', 'cff-version: 1.2.0\ntitle: anymd\nversion: 1.0.0\n');
  return root;
}
const snapshot = (root, ...files) => files.map((f) => readFileSync(join(root, f), 'utf8'));

test('version check passes on a consistent tree and set rewrites every shape', () => {
  const root = versionTree();
  try {
    assert.deepEqual(versionCheck(descriptor, root).problems, []);
    const changed = versionSet(descriptor, '1.2.3', root);
    assert.ok(changed.includes('Cargo.lock') && changed.includes('CITATION.cff') && changed.includes('packages/aliases/alias/package.json'));
    assert.deepEqual(versionCheck(descriptor, root), { want: '1.2.3', problems: [] });
    const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8');
    assert.equal(cargo, CARGO.replace('version = "1.0.0" # product', 'version = "1.2.3" # product').replace(/(anymd(?:-core)? = \{[^}]*version = ")1\.0\.0/g, '$11.2.3'));
    assert.match(cargo, /# workspace root/);
    assert.match(cargo, /serde = \{ version = "1\.0\.0" \}/);
    const alias = JSON.parse(readFileSync(join(root, 'packages/aliases/alias/package.json'), 'utf8'));
    assert.deepEqual(alias.dependencies, { '@sylphx/anymd': '1.2.3', other: '9.9.9' });
    assert.ok(readFileSync(join(root, 'server.json'), 'utf8').endsWith('}\n'));
    assert.match(readFileSync(join(root, 'server.json'), 'utf8'), /\n  "version": "1\.2\.3"/);
    assert.equal(readFileSync(join(root, 'CITATION.cff'), 'utf8'), 'cff-version: 1.2.0\ntitle: anymd\nversion: 1.2.3\n');
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('version set leaves independent fork versions and registry packages unchanged', () => {
  const root = versionTree();
  try {
    versionSet(descriptor, '2.0.0', root);
    const cargo = readFileSync(join(root, 'Cargo.toml'), 'utf8');
    assert.match(cargo, /anymd-pdf-extract = \{ path = "forks\/pdf", version = "0\.12\.2" \}/);
    const lock = readFileSync(join(root, 'Cargo.lock'), 'utf8');
    assert.match(lock, /name = "anymd-oar-ocr-vl"\nversion = "0\.9\.2"/);
    assert.match(lock, /name = "serde"\nversion = "1\.0\.0"/);
    assert.match(lock, /name = "anymd"\nversion = "2\.0\.0"/);
    assert.deepEqual(versionCheck(descriptor, root).problems, []);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('version set refuses when a product location overlaps an independent one', () => {
  const root = versionTree();
  try {
    const overlap = { ...descriptor, independent: [{ file: 'Cargo.toml', kind: 'toml', fields: ['workspace.dependencies.anymd.version'] }] };
    assert.throws(() => versionSet(overlap, '2.0.0', root), /independent version changed/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('version check names the file and field of a drifted pin', () => {
  const root = versionTree({ cargo: CARGO.replace('anymd-core = { path = "crates/core", version = "1.0.0" }', 'anymd-core = { path = "crates/core", version = "0.9.0" }') });
  try {
    const { problems } = versionCheck(descriptor, root);
    assert.deepEqual(problems, ['Cargo.toml workspace.dependencies.anymd-core.version: 0.9.0, want 1.0.0']);
    writeFileSync(join(root, 'packages/npm/linux/package.json'), pkg({ name: 'x', version: '0.8.0' }));
    assert.ok(versionCheck(descriptor, root).problems.includes('packages/npm/linux/package.json version: 0.8.0, want 1.0.0'));
    assert.deepEqual(versionCheck(descriptor, root, '1.0.0').want, '1.0.0');
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('version check flags an independent fork that left its expected version', () => {
  const root = versionTree({ cargo: CARGO.replace('0.12.2', '1.0.0') });
  try {
    assert.deepEqual(versionCheck(descriptor, root).problems, ['Cargo.toml workspace.dependencies.anymd-pdf-extract.version: independent version is 1.0.0, expected 0.12.2']);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('version set is idempotent and touches no file when already current', () => {
  const root = versionTree();
  const files = ['Cargo.toml', 'Cargo.lock', 'server.json', 'CITATION.cff', 'packages/anymd/package.json'];
  try {
    versionSet(descriptor, '3.1.4', root);
    const first = snapshot(root, ...files);
    assert.deepEqual(versionSet(descriptor, '3.1.4', root), []);
    assert.deepEqual(snapshot(root, ...files), first);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('version descriptor errors name the file and what is missing', () => {
  const root = versionTree();
  try {
    const gone = { ...descriptor, product: [{ file: 'server.json', kind: 'json', fields: ['nope'] }] };
    assert.throws(() => versionCheck(gone, root), /server\.json: field nope not found/);
    const nofile = { ...descriptor, product: [{ file: 'missing/*.json', kind: 'json', fields: ['version'] }] };
    assert.throws(() => versionCheck(nofile, root), /missing\/\*\.json: no file matches/);
    const nolock = { ...descriptor, product: [{ file: 'Cargo.lock', kind: 'cargo-lock', packages: ['serde'] }] };
    assert.throws(() => versionCheck(nolock, root), /workspace package serde not found/);
    assert.throws(() => versionSet(descriptor, 'v1', root), /X\.Y\.Z/);
    writeFileSync(join(root, 'd.json'), JSON.stringify({ source: descriptor.source }));
    assert.throws(() => readDescriptor(join(root, 'd.json')), /at least one of product, pins/);
    assert.deepEqual(expandFiles(root, 'packages/npm/*/package.json'), ['packages/npm/darwin/package.json', 'packages/npm/linux/package.json']);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('version CLI sets and checks through the descriptor file', () => {
  const root = versionTree();
  const run = (...a) => execFileSync(process.execPath, [join(import.meta.dirname, 'release.mjs'), 'version', ...a], { cwd: root, encoding: 'utf8' });
  try {
    writeFileSync(join(root, 'version-manifests.json'), JSON.stringify(descriptor));
    assert.match(run('check'), /at 1\.0\.0/);
    assert.match(run('set', '1.1.0'), /set 1\.1\.0/);
    assert.match(run('check'), /at 1\.1\.0/);
    assert.match(run('set', '1.1.0'), /already at 1\.1\.0/);
    assert.throws(() => run('check', '9.9.9', '--descriptor', 'version-manifests.json'), /version drift/);
  } finally { rmSync(root, { recursive: true, force: true }); }
});
