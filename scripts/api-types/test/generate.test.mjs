import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import { gunzipSync } from 'node:zlib';
import {
  ApiTypesError, FORBIDDEN_PACKAGE_KEYS, PACKAGE_FILES, PROVENANCE_KEYS, TAR_MTIME, buildPackage, generate, inspectTarball, parseSidecar,
  payloadSha256, readTar, sha256, withUmask,
} from '../lib.mjs';
import { cloneRepo, fixtureContract, git, makeDirectory, makeRepo, pinnedNode, realTool, serialise } from './helpers.mjs';

const build = (fixture, overrides = {}) => generate({
  repo: fixture.repo, tool: fixture.tool, output: fixture.output, version: '0.0.0-ci', nodeVersion: pinnedNode, npmVersion: 'test', ...overrides,
});
const tarballOf = (fixture, result) => readFileSync(join(fixture.output, result.filename));

// T2. A generator that embeds run-specific data must be refused; without this the check could be vacuous.
test('non-deterministic declarations are refused and nothing is written', (t) => {
  const fixture = makeRepo(t);
  let runs = 0;
  const unstable = (_input, output) => writeFileSync(output, `// generated at run ${runs++}\nexport type paths = {};\n`);
  assert.throws(() => build(fixture, { generator: unstable }), /Non-deterministic API declarations/);
  assert.equal(existsSync(fixture.output), false);
  const random = (_input, output) => writeFileSync(output, `// ${Math.random()}\n`);
  assert.throws(() => build(fixture, { generator: random }), /Non-deterministic API declarations/);
  assert.equal(existsSync(fixture.output), false);
});

// A builder that trusts the ambient umask, as the base implementation did.
function leakyBuilder({ root, version, declarations, schema, license, provenance }) {
  const directory = join(root, 'package');
  mkdirSync(directory, { recursive: true });
  mkdirSync(join(root, 'packed'));
  const manifest = { name: '@atlas/api-types', version, license: 'AGPL-3.0-only', types: './index.d.ts', files: ['index.d.ts', 'openapi.json', 'provenance.json', 'LICENSE'] };
  writeFileSync(join(directory, 'index.d.ts'), declarations);
  writeFileSync(join(directory, 'openapi.json'), schema);
  writeFileSync(join(directory, 'LICENSE'), license);
  writeFileSync(join(directory, 'provenance.json'), JSON.stringify(provenance));
  writeFileSync(join(directory, 'package.json'), JSON.stringify(manifest));
  const [{ filename }] = JSON.parse(execFileSync('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', join(root, 'packed')], { cwd: directory, encoding: 'utf8' }));
  return { filename, bytes: readFileSync(join(root, 'packed', filename)) };
}

// T3. Regression for the umask defect: same commit and version, different umask, different checkout, different TMPDIR.
test('the tarball does not depend on umask, source file modes, checkout path or TMPDIR', (t) => {
  const fixture = makeRepo(t, { licenseMode: 0o600 });
  const inputs = { version: '0.0.0-ci', declarations: Buffer.from('export type paths = {};\n'), schema: Buffer.from('{}\n'), license: Buffer.from('licence\n'), provenance: { a: 1 } };
  const packed = [0o022, 0o077, 0o002].map((mask) => withUmask(mask, () => buildPackage({ ...inputs, root: makeDirectory(t) })));
  for (const other of packed) assert.ok(other.bytes.equals(packed[0].bytes), 'umask changed the tarball');
  for (const entry of readTar(gunzipSync(packed[0].bytes))) {
    assert.deepEqual([entry.mode, entry.uid, entry.gid, entry.mtime], [0o644, 0, 0, TAR_MTIME], entry.name);
  }
  // The negative control: a builder that trusts the ambient umask is caught by the double build.
  assert.throws(() => build(fixture, { builder: leakyBuilder }), /Non-deterministic API package/);
  assert.equal(existsSync(fixture.output), false);
  // End to end: a 0600 LICENSE, another TMPDIR and another checkout give the same bytes.
  const first = build(fixture);
  const otherTmp = makeDirectory(t);
  const previous = process.env.TMPDIR;
  process.env.TMPDIR = otherTmp;
  let second;
  try {
    second = withUmask(0o077, () => build(cloneRepo(t, fixture)));
  } finally {
    if (previous === undefined) delete process.env.TMPDIR; else process.env.TMPDIR = previous;
  }
  assert.equal(second.sha256, first.sha256);
  assert.equal(second.tar_payload_sha256, first.tar_payload_sha256);
});

// T4. The handover's package shape.
test('the package is declarations only with the frozen file and key sets', (t) => {
  const fixture = makeRepo(t);
  const result = build(fixture);
  const tarball = tarballOf(fixture, result);
  const { entries, manifest, provenance } = inspectTarball(tarball, result.filename);
  assert.deepEqual([...entries.keys()].sort(), PACKAGE_FILES.map((name) => `package/${name}`).sort());
  assert.deepEqual(Object.keys(manifest), ['name', 'version', 'license', 'types', 'exports', 'files']);
  for (const key of FORBIDDEN_PACKAGE_KEYS) assert.ok(!(key in manifest), key);
  assert.deepEqual(Object.keys(provenance), PROVENANCE_KEYS);
  assert.equal(provenance.api_version, '0.12.0');
  assert.equal(provenance.generator, 'openapi-typescript@7.4.3');
  assert.equal(provenance.contract_sha256, sha256(readFileSync(join(fixture.repo, 'api/openapi.json'))));
  assert.equal(provenance.backend_commit, git(fixture.repo, 'rev-parse', 'HEAD').trim());
  assert.equal(provenance.generator_lock_sha256, sha256(readFileSync(join(realTool, 'package-lock.json'))));
  assert.match(entries.get('package/index.d.ts').data.toString(), /export interface paths/);
  assert.equal(result.filename, 'atlas-api-types-0.0.0-ci.tgz');
  const sidecar = readFileSync(join(fixture.output, `${result.filename}.sha256`), 'utf8');
  assert.deepEqual(parseSidecar(sidecar), { checksum: sha256(tarball), filename: result.filename });
  assert.equal(result.tar_payload_sha256, payloadSha256(tarball));
  assert.deepEqual(readdirSync(fixture.output).sort(), [result.filename, `${result.filename}.sha256`]);
  // The sidecar must work with the tools a consumer will actually use.
  const check = spawnSync('sh', ['-c', 'command -v sha256sum >/dev/null && sha256sum -c "$1" || shasum -a 256 -c "$1"', 'sh', `${result.filename}.sha256`], { cwd: fixture.output, encoding: 'utf8' });
  assert.equal(check.status, 0, check.stderr);
});

test('the generator lock forbids install scripts and the frontend-owned fetch wrapper', () => {
  const lock = JSON.parse(readFileSync(join(realTool, 'package-lock.json'), 'utf8'));
  const manifest = JSON.parse(readFileSync(join(realTool, 'package.json'), 'utf8'));
  assert.equal(lock.lockfileVersion, 3);
  assert.deepEqual(Object.keys(manifest.devDependencies).sort(), ['openapi-typescript', 'typescript']);
  assert.equal(manifest.dependencies, undefined);
  for (const value of Object.values(manifest.devDependencies)) assert.match(value, /^\d+\.\d+\.\d+$/, 'generator tooling must be exactly pinned');
  for (const [path, entry] of Object.entries(lock.packages)) {
    if (path === '') continue;
    assert.ok(!path.includes('openapi-fetch'), 'openapi-fetch belongs to the frontend lockfile');
    assert.ok(entry.integrity && entry.resolved?.startsWith('https://registry.npmjs.org/'), `${path} is not integrity-locked`);
    assert.ok(!entry.hasInstallScript, `${path} has an install script`);
  }
  assert.equal(manifest.engines.node, readFileSync(join(realTool, '.node-version'), 'utf8').trim());
});

// T5. Release gates.
test('release builds require a clean tracked tree', (t) => {
  const fixture = makeRepo(t);
  writeFileSync(join(fixture.repo, 'stray-untracked.txt'), 'ignored\n');
  assert.equal(build(fixture, { version: '0.12.0', release: true }).version, '0.12.0');
  for (const file of ['scripts/api-types/package-lock.json', 'scripts/api-types/lib.mjs', 'LICENSE']) {
    const path = join(fixture.repo, file);
    const original = readFileSync(path);
    writeFileSync(path, Buffer.concat([original, Buffer.from('\n// edited\n')]));
    const output = join(makeDirectory(t), 'out');
    assert.throws(() => build(fixture, { version: '0.12.0', release: true, output }), /clean tracked working tree/, file);
    assert.equal(existsSync(output), false);
    // The same edit is tolerated when the build is explicitly not a release.
    build(fixture, { output });
    writeFileSync(path, original);
  }
});

test('release versions must carry the contract version, prereleases included', (t) => {
  const fixture = makeRepo(t);
  for (const version of ['0.12.0', '0.12.0-rc.1', '0.12.0-rc.2']) {
    const result = build(fixture, { version, release: true, output: join(makeDirectory(t), 'out') });
    assert.equal(result.filename, `atlas-api-types-${version}.tgz`);
    assert.equal(result.api_version, '0.12.0');
  }
  for (const version of ['0.1.0', '0.13.0-rc.1']) {
    const output = join(makeDirectory(t), 'out');
    assert.throws(() => build(fixture, { version, release: true, output }), (error) => error instanceof ApiTypesError && error.message.includes(version) && error.message.includes('0.12.0'));
    assert.equal(existsSync(output), false);
  }
  assert.throws(() => build(fixture, { version: '1.2.3' }), /Non-release builds must use 0\.0\.0-<label>/);
  assert.throws(() => build(fixture, { version: '0.12.0' }), /Non-release builds must use 0\.0\.0-<label>/);
  assert.throws(() => build(fixture, { version: 'v1.2.3', release: true }), /not MAJOR\.MINOR\.PATCH/);
});

test('release builds require the pinned Node version', (t) => {
  const fixture = makeRepo(t);
  assert.throws(() => build(fixture, { version: '0.12.0', release: true, nodeVersion: '25.8.2' }), /require Node 22\.23\.2.*running 25\.8\.2/);
  assert.equal(existsSync(fixture.output), false);
  build(fixture, { nodeVersion: '25.8.2' });
});

test('an output directory keeps unrelated older assets', (t) => {
  const fixture = makeRepo(t);
  mkdirSync(fixture.output, { recursive: true });
  writeFileSync(join(fixture.output, 'atlas-api-types-9.9.9.tgz'), 'older');
  build(fixture);
  assert.equal(readFileSync(join(fixture.output, 'atlas-api-types-9.9.9.tgz'), 'utf8'), 'older');
});

// T6. Regression for the 1 MiB child-process buffer.
test('a contract over 1 MiB is generated', (t) => {
  const contract = fixtureContract({ padding: 4000 });
  const fixture = makeRepo(t, { contract });
  assert.ok(readFileSync(join(fixture.repo, 'api/openapi.json')).length > 1024 * 1024);
  const result = build(fixture);
  assert.equal(result.contract_sha256, sha256(Buffer.from(serialise(contract))));
});

// T7. Contract guards.
test('the contract must equal HEAD and be valid', (t) => {
  const fixture = makeRepo(t);
  const path = join(fixture.repo, 'api/openapi.json');
  writeFileSync(path, serialise(fixtureContract({ extraPath: true })));
  assert.throws(() => build(fixture), /Working contract differs from recorded commit/);
  assert.equal(existsSync(fixture.output), false);

  for (const [contract, pattern] of [
    [{ ...fixtureContract(), info: { title: 'Fixture' } }, /info\.version "undefined" is missing or not strict SemVer/],
    [fixtureContract({ version: 'v1' }), /info\.version "v1"/],
    [fixtureContract({ version: '1.0.0+build' }), /info\.version "1\.0\.0\+build"/],
  ]) {
    assert.throws(() => build(makeRepo(t, { contract })), pattern);
  }
  assert.throws(() => build(makeRepo(t, { contractText: '{ not json' })), /not valid JSON/);
});

test('a directory that is not a git checkout fails clearly', (t) => {
  const fixture = makeRepo(t);
  const elsewhere = makeDirectory(t);
  assert.throws(() => build(fixture, { repo: elsewhere }), /git rev-parse failed/);
});

test('the pinned generator must be the installed one', (t) => {
  const fixture = makeRepo(t);
  const manifestPath = join(fixture.tool, 'package.json');
  const manifest = JSON.parse(readFileSync(manifestPath, 'utf8'));
  manifest.devDependencies['openapi-typescript'] = '^7.4.3';
  writeFileSync(manifestPath, JSON.stringify(manifest));
  assert.throws(() => build(fixture), /pinned to an exact version/);
  manifest.devDependencies['openapi-typescript'] = '7.4.2';
  writeFileSync(manifestPath, JSON.stringify(manifest));
  assert.throws(() => build(fixture), /Installed openapi-typescript 7\.4\.3 differs from the 7\.4\.2 pin/);
});

// The command line, run inside a fixture so it never depends on the real working tree.
test('the CLI reports usage errors with status 2 and one JSON line on success', (t) => {
  const fixture = makeRepo(t);
  const cli = (...args) => spawnSync(process.execPath, [join(fixture.tool, 'generate.mjs'), ...args], { encoding: 'utf8' });
  for (const args of [[], ['--version'], ['--version', '0.0.0-ci', '--version', '0.0.0-dev'], ['--version', '0.0.0-ci', '--nope'], ['--version', '0.0.0-ci', 'stray'], ['--release']]) {
    const result = cli(...args);
    assert.equal(result.status, 2, `${args.join(' ')}: ${result.stderr}`);
    assert.match(result.stderr, /usage: node scripts\/api-types\/generate\.mjs/);
    assert.equal(result.stdout, '');
  }
  const refused = cli('--version', '1.2.3');
  assert.equal(refused.status, 1);
  assert.match(refused.stderr, /^api-types: Non-release builds must use 0\.0\.0-<label>/);
  const success = cli('--version', '0.0.0-ci');
  assert.equal(success.status, 0, success.stderr);
  assert.equal(success.stdout.trim().split('\n').length, 1, 'stdout must be exactly one line');
  const summary = JSON.parse(success.stdout);
  assert.deepEqual(Object.keys(summary), ['filename', 'sha256', 'tar_payload_sha256', 'version', ...PROVENANCE_KEYS, 'node', 'npm']);
  assert.ok(existsSync(join(fixture.repo, 'target/api-types', summary.filename)));
  const verified = spawnSync(process.execPath, [join(fixture.tool, 'verify.mjs'), join(fixture.repo, 'target/api-types', summary.filename)], { encoding: 'utf8' });
  assert.equal(verified.status, 0, verified.stderr);
});
