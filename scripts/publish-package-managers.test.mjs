import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { execFileSync, spawnSync } from 'node:child_process';

const script = resolve(import.meta.dirname, 'publish-package-managers.sh');
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
  }
  writeFileSync(join(dir, 'bin/git'), `#!/bin/bash
set -euo pipefail
if [[ " $* " == *" clone "* || " $* " == *" push "* ]]; then
  if [[ " $* $PWD " == *fixture/tap* ]]; then expected=fixture-tap; else expected=fixture-bucket; fi
  [ "$GH_TOKEN" = "$expected" ] || { echo wrong-destination-credential >&2; exit 89; }
fi
exec '${git}' -c 'url.file://${dir}/remotes/.insteadOf=https://github.com/' "$@"
`, { mode: 0o755 });
  writeFileSync(join(dir, 'bin/gh'), `#!/bin/bash
set -euo pipefail
[ "$1 $2" = 'release download' ] || exit 90
[ "\${FAIL_DOWNLOAD:-}" != yes ] || exit 44
while [ "$1" != --dir ]; do shift; done
out=$2
for key in darwin-arm64 darwin-x64 linux-arm64-gnu linux-x64-gnu; do
  printf 'fixture %s' "$key" > "$out/tool-$key.tar.gz"
done
[ "\${MISSING_ARCHIVE:-}" = yes ] || printf 'fixture windows' > "$out/tool-win32-x64-msvc.zip"
`, { mode: 0o755 });
  const env = { ...process.env, PATH: `${dir}/bin:${process.env.PATH}`, RUNNER_TEMP: join(dir, 'runner'), GITHUB_WORKSPACE: join(dir, 'source'), GITHUB_REPOSITORY: 'fixture/source', GH_TOKEN: 'fixture-source', NAME: 'tool', V: '1.2.3', TAP: 'fixture/tap', BUCKET: 'fixture/bucket', PACKAGE_JSON: '', DESC: 'Fixture', HOMEPAGE: '', LICENSE_ID: 'MIT', VERSION_ARG: 'version', TAP_TOKEN: 'fixture-tap', BUCKET_TOKEN: 'fixture-bucket' };
  const head = repo => execFileSync(git, ['--git-dir', join(dir, 'remotes/fixture', `${repo}.git`), 'rev-parse', 'main'], { encoding: 'utf8' }).trim();
  const run = overrides => spawnSync('bash', [script], { env: { ...env, ...overrides }, cwd: dir, encoding: 'utf8' });
  return { dir, head, run };
}

test('pushes rendered manifests with distinct credentials, preserves no-op heads, and cleans temporary credentials', t => {
  const f = fixture(t);
  const first = f.run();
  assert.equal(first.status, 0, first.stderr);
  const heads = ['tap', 'bucket'].map(f.head);
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
