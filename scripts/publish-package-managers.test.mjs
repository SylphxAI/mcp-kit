import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';

const script = process.env.PACKAGE_PUBLISHER_TEST_SCRIPT || resolve(import.meta.dirname, 'publish-package-managers.sh');
const action = readFileSync(resolve(import.meta.dirname, '../.github/actions/package-managers/action.yml'), 'utf8');
// Execute the action's cleanup command against the pinned Broker helper's
// RUNNER_TEMP/broker-token artifact, including failures before publishing.
const cleanup = action.match(/    - name: Remove temporary Broker token file\n      if: \$\{\{ always\(\) \}\}\n      shell: bash\n      run: (.+)\n?$/)?.[1];
const git = execFileSync('which', ['git'], { encoding: 'utf8' }).trim();
function fixture(t) {
  const dir = mkdtempSync(join(tmpdir(), 'package-manager-push-'));
  t.after(() => rmSync(dir, { recursive: true, force: true }));
  for (const path of ['bin', 'runner', 'source', 'remotes/fixture']) mkdirSync(join(dir, path), { recursive: true });
  for (const repo of ['tap', 'bucket']) {
    execFileSync(git, ['init', '-q', '--bare', '--initial-branch=main', join(dir, 'remotes/fixture', `${repo}.git`)]);
    const seed = join(dir, `seed-${repo}`);
    execFileSync(git, ['init', '-q', '--initial-branch=main', seed]);
    writeFileSync(join(seed, 'README.md'), 'fixture\n');
    execFileSync(git, ['-C', seed, 'add', '.']);
    execFileSync(git, ['-C', seed, '-c', 'user.name=Fixture', '-c', 'user.email=fixture@example.invalid', 'commit', '-qm', 'seed']);
    execFileSync(git, ['-C', seed, 'push', '-q', join(dir, 'remotes/fixture', `${repo}.git`), 'main']);
    writeFileSync(join(dir, 'remotes/fixture', `${repo}.git/hooks/pre-receive`), `#!/bin/bash
while read old new ref; do
  [ "$ref" != refs/heads/main ] || { echo protected-main >&2; exit 1; }
done
`, { mode: 0o755 });
  }
  writeFileSync(join(dir, 'bin/git'), `#!/bin/bash
set -euo pipefail
if [[ " $* " == *" clone "* || " $* " == *" push "* ]]; then
  if [[ " $* $PWD " == *fixture/tap* ]]; then expected=fixture-tap; else expected=fixture-bucket; fi
  [ "$GH_TOKEN" = "$expected" ] || { echo wrong-destination-credential >&2; exit 89; }
fi
exec '${git}' -c 'url.file://${dir}/remotes/.insteadOf=https://github.com/' "$@"
`, { mode: 0o755 });
  writeFileSync(join(dir, 'bin/gh'), String.raw`#!/usr/bin/env node
import { readFileSync, writeFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';
const args = process.argv.slice(2);
const value = key => args[args.indexOf(key) + 1];
const git = (...args) => execFileSync(${JSON.stringify(git)}, args, { encoding: 'utf8' }).trim();
if (args[0] === 'release' && args[1] === 'download') {
  if (process.env.FAIL_DOWNLOAD === 'yes') process.exit(44);
  const out = value('--dir');
  for (const key of ['darwin-arm64', 'darwin-x64', 'linux-arm64-gnu', 'linux-x64-gnu']) writeFileSync(out + '/tool-' + key + '.tar.gz', 'fixture ' + key);
  if (process.env.MISSING_ARCHIVE !== 'yes') writeFileSync(out + '/tool-win32-x64-msvc.zip', 'fixture windows');
  process.exit(0);
}
const repo = args[0] === 'api' ? args[1].split('/').slice(1, 3).join('/') : value('--repo');
if (!['fixture/tap', 'fixture/bucket'].includes(repo)) process.exit(90);
if (process.env.GH_TOKEN !== 'fixture-' + repo.split('/')[1]) process.exit(89);
const remote = ${JSON.stringify(dir)} + '/remotes/' + repo + '.git';
const stateFile = remote + '/pr.json';
const state = () => {
  try {
    const pr = JSON.parse(readFileSync(stateFile));
    if (pr.state === 'OPEN') pr.headRefOid = git('--git-dir', remote, 'rev-parse', pr.headRefName);
    return pr;
  } catch { return null; }
};
const emit = data => process.stdout.write(JSON.stringify(data) + '\n');
if (args[0] === 'pr' && args[1] === 'list') { const pr = state(); emit(pr ? [pr] : []); }
else if (args[0] === 'pr' && args[1] === 'create') {
  if (state()) process.exit(91);
  const head = value('--head');
  const pr = { number: 1, state: 'OPEN', headRefOid: git('--git-dir', remote, 'rev-parse', head), headRefName: head, baseRefName: 'main', mergeCommit: null };
  writeFileSync(stateFile, JSON.stringify(pr));
  process.stdout.write('https://github.com/' + repo + '/pull/1\n');
}
else if (args[0] === 'pr' && args[1] === 'merge') {
  const pr = state();
  if (value('--match-head-commit') !== pr.headRefOid || !args.includes('--auto') || !args.includes('--squash')) process.exit(92);
  pr.state = process.env.PR_OUTCOME === 'closed' ? 'CLOSED' : process.env.PR_OUTCOME === 'pending' ? 'OPEN' : 'MERGED';
  if (pr.state === 'MERGED') {
    git('--git-dir', remote, 'update-ref', 'refs/heads/main', pr.headRefOid);
    pr.mergeCommit = { oid: pr.headRefOid };
  }
  writeFileSync(stateFile, JSON.stringify(pr));
}
else if (args[0] === 'pr' && args[1] === 'view') {
  const pr = state();
  if (process.env.CHANGED_HEAD === 'yes') pr.headRefOid = 'unexpected';
  emit(pr);
}
else if (args[0] === 'api') {
  const endpoint = args.find(arg => arg.startsWith('repos/'));
  const match = endpoint.match(/contents\/(.+)\?ref=(.+)$/);
  if (!match) process.exit(93);
  const content = process.env.WRONG_BYTES === 'yes' ? 'wrong bytes' : git('--git-dir', remote, 'show', match[2] + ':' + match[1]) + '\n';
  emit({ encoding: 'base64', content: Buffer.from(content).toString('base64') });
}
else process.exit(90);
`, { mode: 0o755 });
  const env = { ...process.env, PATH: `${dir}/bin:${process.env.PATH}`, RUNNER_TEMP: join(dir, 'runner'), GITHUB_WORKSPACE: join(dir, 'source'), GITHUB_REPOSITORY: 'fixture/source', GH_TOKEN: 'fixture-source', NAME: 'tool', V: '1.2.3', TAP: 'fixture/tap', BUCKET: 'fixture/bucket', PACKAGE_JSON: '', DESC: 'Fixture', HOMEPAGE: '', LICENSE_ID: 'MIT', VERSION_ARG: 'version', TAP_TOKEN: 'fixture-tap', BUCKET_TOKEN: 'fixture-bucket', PACKAGE_MANAGERS_WAIT_SECONDS: '0', PACKAGE_MANAGERS_POLL_SECONDS: '0' };
  const head = repo => execFileSync(git, ['--git-dir', join(dir, 'remotes/fixture', `${repo}.git`), 'rev-parse', 'main'], { encoding: 'utf8' }).trim();
  const run = overrides => spawnSync('bash', [script], { env: { ...env, ...overrides }, cwd: dir, encoding: 'utf8' });
  const runAction = (overrides = {}, authorizationFailed = false) => {
    assert.ok(cleanup, 'the final composite step must clean up even after an earlier failure');
    // The helper can leave this file after either authorization step, before
    // the publisher script has started and installed its own EXIT trap.
    writeFileSync(join(env.RUNNER_TEMP, 'broker-token'), 'fixture-broker-token', { mode: 0o600 });
    const result = authorizationFailed ? { status: 1 } : run(overrides);
    const cleaned = spawnSync('bash', ['-euc', cleanup], { env, cwd: dir, encoding: 'utf8' });
    assert.equal(cleaned.status, 0, cleaned.stderr);
    return result;
  };
  return { dir, head, run, runAction, pr: repo => JSON.parse(readFileSync(join(dir, 'remotes/fixture', `${repo}.git/pr.json`))) };
}

for (const [name, override, authorizationFailed] of [
  ['successful publication', {}, false],
  ['publisher failure', { FAIL_DOWNLOAD: 'yes' }, false],
  ['authorization failure before publication', {}, true],
]) {
  test(`composite removes the Broker helper file after ${name}`, t => {
    const f = fixture(t);
    const before = ['tap', 'bucket'].map(f.head);
    const result = f.runAction(override, authorizationFailed);
    assert.equal(result.status === 0, name === 'successful publication');
    assert.deepEqual(readdirSync(join(f.dir, 'runner')), []);
    if (result.status !== 0) assert.deepEqual(['tap', 'bucket'].map(f.head), before);
  });
}

test('merges rendered manifests through protected-main PRs with distinct credentials, preserves no-op heads, and cleans temporary credentials', t => {
  const f = fixture(t);
  const first = f.run();
  assert.equal(first.status, 0, first.stderr);
  const heads = ['tap', 'bucket'].map(f.head);
  for (const repo of ['tap', 'bucket']) assert.equal(f.pr(repo).state, 'MERGED');
  const second = f.run();
  assert.equal(second.status, 0, second.stderr);
  assert.deepEqual(['tap', 'bucket'].map(f.head), heads);
  assert.match(second.stdout, /already at 1.2.3/);
  assert.deepEqual(readdirSync(join(f.dir, 'runner')), []);
  for (const repo of ['tap', 'bucket']) assert.doesNotMatch(readFileSync(join(f.dir, 'remotes/fixture', `${repo}.git/config`), 'utf8'), /fixture-(tap|bucket)/);
});
for (const [name, override] of [['release download refused', { FAIL_DOWNLOAD: 'yes' }], ['archive missing', { MISSING_ARCHIVE: 'yes' }], ['destination credential missing', { BUCKET_TOKEN: '' }]]) {
  test(`${name} cannot write either destination`, t => {
    const f = fixture(t);
    const before = ['tap', 'bucket'].map(f.head);
    const result = f.run(override);
    assert.notEqual(result.status, 0);
    assert.deepEqual(['tap', 'bucket'].map(f.head), before);
    assert.deepEqual(readdirSync(join(f.dir, 'runner')), []);
  });
}

for (const [name, override, reason] of [
  ['closed PR', { PR_OUTCOME: 'closed' }, /PR is CLOSED/],
  ['merge deadline', { PR_OUTCOME: 'pending' }, /did not merge before deadline/],
  ['changed PR head', { CHANGED_HEAD: 'yes' }, /PR head changed/],
  ['merged bytes differ', { WRONG_BYTES: 'yes' }, /merged manifest differs/],
]) {
  test(`${name} fails publication`, t => {
    const f = fixture(t);
    const result = f.run(override);
    assert.notEqual(result.status, 0, result.stdout + result.stderr);
    assert.match(result.stdout + result.stderr, reason);
    assert.deepEqual(readdirSync(join(f.dir, 'runner')), []);
  });
}

test('a pending PR resumes without a duplicate branch or PR', t => {
  const f = fixture(t);
  assert.notEqual(f.run({ PR_OUTCOME: 'pending' }).status, 0);
  const first = f.pr('tap');
  const result = f.run();
  assert.equal(result.status, 0, result.stderr);
  assert.equal(f.pr('tap').headRefOid, first.headRefOid);
  assert.equal(f.pr('tap').number, first.number);
});

test('a pending PR receives a corrected manifest through a fast-forward update', t => {
  const f = fixture(t);
  assert.notEqual(f.run({ PR_OUTCOME: 'pending', DESC: 'Original description' }).status, 0);
  const first = f.pr('tap');
  const result = f.run({ DESC: 'Corrected description.' });
  assert.equal(result.status, 0, result.stderr);
  assert.equal(f.pr('tap').number, first.number);
  assert.notEqual(f.pr('tap').headRefOid, first.headRefOid);
});
