import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import { assertPolicy, isAcceptedVersion, parseSemver } from '../lib.mjs';
import { makeDirectory, pinnedNode, realBackend } from './helpers.mjs';

const valid = [
  '0.0.0-dev', '0.0.0-ci', '0.0.0-bootstrap.fe4dba10', '0.12.0', '1.2.3', '10.20.30', '1.0.0-rc.1', '1.0.0-alpha-1',
  '1.0.0-0', '1.0.0-x.7.z.92', '1.0.0--', `1.0.0-${'a'.repeat(113)}`,
];
const invalid = [
  '', '1.2', 'v1.2.3', '01.2.3', '1.02.3', '1.2.03', '1.2.3+build', '1.2.3-rc.1+build', '1.2.3-', '1.2.3-01', '1.2.3-rc.01',
  '1.2.3-rc..1', '1.2.3-rc.', ' 1.2.3', '1.2.3 ', '1.2.3\n', '1.2.3.4', 'a.b.c', '1.2.3-é', `1.0.0-${'a'.repeat(114)}`,
];

// T1. The tag grammar in the Release workflow and the CLI's grammar must be one and the same.
function workflowAccepts(script, candidate, directory) {
  const output = join(directory, 'output');
  writeFileSync(output, '');
  const result = spawnSync('python3', ['-'], {
    input: script, encoding: 'utf8',
    env: { ...process.env, EVENT_NAME: 'push', REF_NAME: `v${candidate}`, GITHUB_OUTPUT: output },
  });
  if (result.error) throw result.error;
  return result.status === 0 && readFileSync(output, 'utf8') === `version=${candidate}\n`;
}

function workflowScript() {
  const lines = readFileSync(join(realBackend, '.github/workflows/release.yml'), 'utf8').split('\n');
  const start = lines.findIndex((line) => /^\s*python3 - <<'PYTHON'$/.test(line));
  assert.notEqual(start, -1, 'metadata job no longer embeds its Python tag check');
  const end = lines.findIndex((line, index) => index > start && line.trim() === 'PYTHON');
  const body = lines.slice(start + 1, end);
  const indent = Math.min(...body.filter((line) => line.trim()).map((line) => line.match(/^ */)[0].length));
  return body.map((line) => line.slice(indent)).join('\n') + '\n';
}

test('the SemVer table matches the Release workflow tag grammar', (t) => {
  const script = workflowScript();
  const directory = makeDirectory(t);
  const probe = spawnSync('python3', ['--version']);
  if (probe.error) return t.skip('python3 is unavailable');
  for (const candidate of valid) {
    assert.equal(isAcceptedVersion(candidate), true, `CLI should accept ${JSON.stringify(candidate)}`);
    assert.equal(workflowAccepts(script, candidate, directory), true, `workflow should accept ${JSON.stringify(candidate)}`);
  }
  for (const candidate of invalid) {
    assert.equal(isAcceptedVersion(candidate), false, `CLI should refuse ${JSON.stringify(candidate)}`);
    assert.equal(workflowAccepts(script, candidate, directory), false, `workflow should refuse ${JSON.stringify(candidate)}`);
  }
});

test('parseSemver exposes the core and prerelease', () => {
  assert.deepEqual(parseSemver('0.12.0-rc.1'), { major: '0', minor: '12', patch: '0', prerelease: 'rc.1', core: '0.12.0' });
  assert.equal(parseSemver('1.2.3').prerelease, null);
  assert.equal(parseSemver(undefined), null);
});

test('policy enforces the release and non-release version rules', () => {
  const base = { apiVersion: '0.12.0', nodeVersion: pinnedNode, pinnedNode };
  for (const version of ['0.0.0-dev', '0.0.0-ci', '0.0.0-bootstrap.abc']) assertPolicy({ ...base, version, release: false });
  for (const version of ['1.2.3', '0.12.0', '0.0.0', '0.0.1-ci']) {
    assert.throws(() => assertPolicy({ ...base, version, release: false }), /Non-release builds must use 0\.0\.0-<label>/);
  }
  for (const version of ['0.12.0', '0.12.0-rc.1', '0.12.0-rc.2']) assertPolicy({ ...base, version, release: true });
  for (const version of ['0.1.0', '0.13.0-rc.1', '1.0.0']) {
    assert.throws(() => assertPolicy({ ...base, version, release: true }), (error) => error.message.includes(version) && error.message.includes('0.12.0'));
  }
  assert.throws(() => assertPolicy({ ...base, version: '0.0.0-dev', release: true }), /info\.version is 0\.12\.0/);
  assert.throws(() => assertPolicy({ ...base, version: '0.12.0', release: true, nodeVersion: '25.8.2' }), /require Node 22\.23\.2/);
  assert.throws(() => assertPolicy({ ...base, version: '0.12.0', release: true, apiVersion: 'v1' }), /not strict SemVer/);
  assert.throws(() => assertPolicy({ ...base, version: '0.12.0', release: true, apiVersion: undefined }), /not strict SemVer/);
});
