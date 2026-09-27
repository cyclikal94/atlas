import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test, { after, before } from 'node:test';
import { gunzipSync } from 'node:zlib';
import { ApiTypesError, UsageError, consumerSource, contractSurface, generate, readTar, sha256, verifyAsset } from '../lib.mjs';
import { fixtureContract, git, makeRepo, pinnedNode, serialise, writeTar } from './helpers.mjs';

let shared;
const cleanup = [];
const scope = { after: (action) => cleanup.push(action) };

/** Generates a package from `fixture` (a diagnostic by default) and returns where it landed. */
function build(fixture, { version = '0.0.0-ci', release = false } = {}) {
  const result = generate({ repo: fixture.repo, tool: fixture.tool, output: fixture.output, version, release, nodeVersion: pinnedNode, npmVersion: 'test' });
  const path = join(fixture.output, result.filename);
  return { fixture, filename: result.filename, path, bytes: readFileSync(path), result };
}

before(() => {
  const fixture = makeRepo(scope);
  const directory = mkdtempSync(join(tmpdir(), 'atlas-api-types-verify-test-'));
  cleanup.push(() => rmSync(directory, { recursive: true, force: true }));
  // The diagnostic and the release packages come from the same commit of the same fixture checkout.
  shared = { fixture, directory, count: 0, ...build(fixture), release: build(fixture, { version: '0.12.0', release: true }) };
});
after(() => cleanup.forEach((action) => action()));

/** Rewrites a good package (the diagnostic unless `base` says otherwise) with `mutate`, saves it beside a matching sidecar and returns its path. */
function damaged(mutate, { name, sidecar, base = shared } = {}) {
  name ??= base.filename;
  const entries = readTar(gunzipSync(base.bytes)).map((entry) => ({ ...entry }));
  const result = mutate(entries);
  const changed = Array.isArray(result) ? result : entries;
  const bytes = writeTar(changed);
  const directory = join(shared.directory, `case-${shared.count += 1}`);
  mkdirSync(directory);
  const path = join(directory, name);
  writeFileSync(path, bytes);
  if (sidecar !== null) writeFileSync(`${path}.sha256`, sidecar ?? `${sha256(bytes)}  ${name}\n`);
  return path;
}

const verify = (tarballPath, extra = {}) => verifyAsset({ tarballPath, tool: shared.fixture.tool, ...extra });
const verifyRelease = (tarballPath, extra = {}) => verify(tarballPath, { release: true, version: '0.12.0', ...extra });
const releasePackage = (mutate, options) => damaged(mutate, { base: shared.release, ...options });
const entryNamed = (entries, name) => entries.find((entry) => entry.name === `package/${name}`);
const editJson = (name, change) => (entries) => {
  const entry = entryNamed(entries, name);
  const value = JSON.parse(entry.data.toString());
  change(value);
  entry.data = Buffer.from(JSON.stringify(value, null, 2) + '\n');
};
const refusal = (path, pattern, run = verify) => assert.throws(() => run(path), (error) => error instanceof ApiTypesError && pattern.test(error.message), String(pattern));

test('a freshly generated package verifies', () => {
  const result = verify(join(shared.fixture.output, shared.filename));
  assert.equal(result.filename, shared.filename);
  assert.equal(result.sha256, sha256(shared.bytes));
  assert.equal(result.api_version, '0.12.0');
  assert.equal(result.source_verified, false, 'a diagnostic verification must not claim verified provenance');
});

// T8. Each control corrupts exactly one property and must be caught for its own reason.
test('verification detects damage to the package contents', () => {
  refusal(damaged((entries) => { const entry = entryNamed(entries, 'index.d.ts'); entry.data = Buffer.from(entry.data); entry.data[40] ^= 0x01; }),
    /index\.d\.ts does not match a fresh generator run/);
  refusal(damaged((entries) => entries.push({ name: 'package/extra.js', data: Buffer.from('x') })), /Unexpected tar entries: package\/extra\.js/);
  refusal(damaged((entries) => entries.filter((entry) => entry.name !== 'package/LICENSE')), /Missing tar entries: package\/LICENSE/);
  refusal(damaged((entries) => entries.push({ name: 'package/', data: Buffer.alloc(0) })), /Unexpected tar entries/);
  for (const key of ['dependencies', 'peerDependencies', 'optionalDependencies', 'scripts', 'main', 'module', 'bin']) {
    refusal(damaged(editJson('package.json', (value) => { value[key] = key === 'main' || key === 'module' || key === 'bin' ? './x.js' : {}; })),
      new RegExp(`forbidden keys: ${key}`));
  }
  refusal(damaged(editJson('package.json', (value) => { value.description = 'x'; })), /package\.json keys must be exactly/);
  refusal(damaged(editJson('package.json', (value) => { value.name = '@atlas/other'; })), /package\.json name is @atlas\/other/);
  refusal(damaged(editJson('package.json', (value) => { value.files.push('src'); })), /does not match the declarations-only manifest/);
  refusal(damaged(editJson('package.json', (value) => { value.version = '1.2'; })), /not accepted release SemVer/);
  refusal(damaged(editJson('package.json', (value) => { value.version = '0.0.0-other'; })), /requires atlas-api-types-0\.0\.0-other\.tgz/);
});

test('verification detects provenance that disagrees with the contract', () => {
  refusal(damaged(editJson('provenance.json', (value) => { value.contract_sha256 = '0'.repeat(64); })), /contract_sha256 does not match the packaged openapi\.json/);
  refusal(damaged(editJson('provenance.json', (value) => { value.api_version = '9.9.9'; })), /api_version 9\.9\.9 differs from the packaged contract's 0\.12\.0/);
  refusal(damaged(editJson('provenance.json', (value) => { value.extra = 1; })), /provenance\.json keys must be exactly/);
  refusal(damaged(editJson('provenance.json', (value) => { value.backend_commit = 'abc'; })), /not a 40-character commit/);
  refusal(damaged(editJson('provenance.json', (value) => { value.generator = 'openapi-typescript@^7'; })), /is not an exact pin/);
  refusal(damaged(editJson('provenance.json', (value) => { value.generator = 'openapi-typescript@7.4.2'; })), /generated with openapi-typescript@7\.4\.2/);
  refusal(damaged((entries) => { entryNamed(entries, 'openapi.json').data = Buffer.from('{"info":{"version":"0.12.0"},"paths":{}}\n'); }),
    /contract_sha256 does not match the packaged openapi\.json/);
});

test('verification detects wrong entry metadata', () => {
  refusal(damaged((entries) => { entries[0].mode = 0o600; }), /has mode 0600, expected 0644/);
  refusal(damaged((entries) => { entries[1].mode = 0o755; }), /has mode 0755, expected 0644/);
  refusal(damaged((entries) => { entries[2].uid = 1000; }), /has owner 1000:0, expected 0:0/);
  refusal(damaged((entries) => { entries[3].gid = 20; }), /has owner 0:20, expected 0:0/);
  refusal(damaged((entries) => { entries[0].mtime = 1_700_000_000; }), /has modification time 1700000000/);
});

test('verification detects a bad checksum sidecar or asset name', () => {
  const good = damaged((entries) => entries);
  const bytes = readFileSync(good);
  refusal(damaged((entries) => entries, { sidecar: `${'0'.repeat(64)}  ${shared.filename}\n` }), /does not match its sidecar/);
  refusal(damaged((entries) => entries, { sidecar: `${sha256(bytes)}  another.tgz\n` }), /sidecar names another\.tgz/);
  for (const sidecar of [`${sha256(bytes).toUpperCase()}  ${shared.filename}\n`, `${sha256(bytes)} ${shared.filename}\n`, `${sha256(bytes)}  ${shared.filename}`, `${sha256(bytes)}  ${shared.filename}\n\n`, '']) {
    refusal(damaged((entries) => entries, { sidecar }), /Checksum sidecar must be exactly/);
  }
  refusal(damaged((entries) => entries, { sidecar: null }), /Checksum sidecar not found/);
  assert.throws(() => verify(join(shared.directory, 'missing.tgz')), /Tarball not found/);
  const renamed = damaged((entries) => entries, { name: 'atlas-api-types-9.9.9.tgz' });
  refusal(renamed, /Asset is named atlas-api-types-9\.9\.9\.tgz but package\.json version 0\.0\.0-ci requires atlas-api-types-0\.0\.0-ci\.tgz/);
  const notGzip = join(shared.directory, 'not-gzip.tgz');
  writeFileSync(notGzip, 'not an archive');
  writeFileSync(`${notGzip}.sha256`, `${sha256(Buffer.from('not an archive'))}  not-gzip.tgz\n`);
  refusal(notGzip, /Not a gzip-compressed tar archive/);
});

// R2. A matching sidecar shows the bytes are unchanged, not where they came from. These packages are
// internally consistent and well-formed, yet misattributed, so only the trusted checkout can refuse them.
test('a release package verifies against the checkout and version it claims', () => {
  const result = verifyRelease(shared.release.path);
  assert.equal(result.source_verified, true);
  assert.equal(result.version, '0.12.0');
  assert.equal(result.backend_commit, git(shared.fixture.repo, 'rev-parse', 'HEAD').trim());
  assert.equal(result.sha256, sha256(shared.release.bytes));
});

test('a release candidate verifies against the same contract as its final release', (t) => {
  const fixture = makeRepo(t);
  const candidate = build(fixture, { version: '0.12.0-rc.1', release: true });
  const check = (extra) => verifyAsset({ tarballPath: candidate.path, tool: fixture.tool, release: true, ...extra });
  assert.equal(check({ version: '0.12.0-rc.1' }).source_verified, true);
  refusal(candidate.path, /differs from the expected version 0\.12\.0\b/, () => check({ version: '0.12.0' }));
});

test('release verification refuses a well-formed package attributed to the wrong source', () => {
  const wrongCommit = releasePackage(editJson('provenance.json', (value) => { value.backend_commit = '0'.repeat(40); }));
  refusal(wrongCommit, /records backend commit 0{40}, but this checkout is at [0-9a-f]{40}; check out the release tag/, verifyRelease);
  // The same package is only a diagnostic to a verifier that was not asked to check provenance, and says so.
  assert.throws(() => verify(wrongCommit), /is a release version; verify it with --release --version 0\.12\.0/);

  const wrongLock = releasePackage(editJson('provenance.json', (value) => { value.generator_lock_sha256 = '0'.repeat(64); }));
  refusal(wrongLock, /generator lock digest 0{64}, but this checkout's scripts\/api-types\/package-lock\.json is [0-9a-f]{64}/, verifyRelease);

  // Self-consistent (its own hash and version agree) but not the contract committed at the checkout's revision.
  const otherContract = releasePackage((entries) => {
    const changed = Buffer.from(serialise(fixtureContract({ extraPath: true })));
    entryNamed(entries, 'openapi.json').data = changed;
    editJson('provenance.json', (value) => { value.contract_sha256 = sha256(changed); })(entries);
  });
  refusal(otherContract, /Packaged openapi\.json differs from api\/openapi\.json at [0-9a-f]{40}/, verifyRelease);
});

test('release verification refuses a release version that does not match the tag or the contract', () => {
  // The package says 0.12.0 but the tag under verification is another version.
  refusal(shared.release.path, /Package version 0\.12\.0 differs from the expected version 0\.12\.1/, (path) => verifyRelease(path, { version: '0.12.1' }));
  // A package and file name both saying 9.9.9 against a 0.12.0 contract, with an expected 9.9.9 tag.
  const mislabelled = releasePackage(editJson('package.json', (value) => { value.version = '9.9.9'; }), { name: 'atlas-api-types-9.9.9.tgz' });
  refusal(mislabelled, /Release version 9\.9\.9 carries core 9\.9\.9, but the contract's info\.version is 0\.12\.0/, (path) => verifyRelease(path, { version: '9.9.9' }));
  // A diagnostic build cannot pass as a release.
  refusal(shared.path, /Release version 0\.0\.0-ci carries core 0\.0\.0, but the contract's info\.version is 0\.12\.0/, (path) => verifyRelease(path, { version: '0.0.0-ci' }));
  // Release verification needs the expected version from the caller, not from the package under test.
  assert.throws(() => verify(shared.release.path, { release: true }), UsageError);
});

test('a wrong commit, lock digest and version together are refused in both modes', () => {
  const composite = damaged((entries) => {
    editJson('provenance.json', (value) => { value.backend_commit = '0'.repeat(40); value.generator_lock_sha256 = '0'.repeat(64); })(entries);
    editJson('package.json', (value) => { value.version = '9.9.9'; })(entries);
  }, { base: shared.release, name: 'atlas-api-types-9.9.9.tgz' });
  refusal(composite, /Release version 9\.9\.9 carries core 9\.9\.9/, (path) => verifyRelease(path, { version: '9.9.9' }));
  refusal(composite, /is a release version; verify it with --release --version 9\.9\.9/);
});

test('a diagnostic package is supported but its provenance is reported as unverified', () => {
  const foreign = damaged(editJson('provenance.json', (value) => { value.backend_commit = '0'.repeat(40); value.generator_lock_sha256 = '0'.repeat(64); }));
  const result = verify(foreign);
  assert.equal(result.source_verified, false);
  assert.equal(result.backend_commit, '0'.repeat(40), 'the recorded digest is reported, never presented as verified');
  assert.equal(verify(shared.path, { version: '0.0.0-ci' }).source_verified, false);
  refusal(shared.path, /differs from the expected version 0\.0\.0-dev/, (path) => verify(path, { version: '0.0.0-dev' }));
});

test('release verification needs a checkout that is at the recorded revision and unmodified', (t) => {
  const moved = makeRepo(t);
  const packaged = build(moved, { version: '0.12.0', release: true });
  const run = () => verifyAsset({ tarballPath: packaged.path, tool: moved.tool, release: true, version: '0.12.0' });
  assert.equal(run().source_verified, true);

  // Changing a tracked input leaves the recorded digests describing something else.
  const licence = join(moved.repo, 'LICENSE');
  const original = readFileSync(licence);
  writeFileSync(licence, Buffer.concat([original, Buffer.from('edited\n')]));
  assert.throws(run, /clean tracked working tree/);
  writeFileSync(licence, original);

  // A checkout at another revision is not the tagged source, even with identical files.
  git(moved.repo, '-c', 'commit.gpgsign=false', 'commit', '--allow-empty', '-q', '-m', 'Later');
  assert.throws(run, /Package records backend commit [0-9a-f]{40}, but this checkout is at [0-9a-f]{40}/);
});

// The surface check exists for a generator that silently drops or invents operations, so simulate one.
function generatorEmitting(declarations) {
  return (_input, output) => writeFileSync(output, declarations);
}
function declarationsFor(t, contractOptions) {
  const other = makeRepo(t, { contract: fixtureContract(contractOptions) });
  const result = generate({ repo: other.repo, tool: other.tool, output: other.output, version: '0.0.0-ci', nodeVersion: pinnedNode, npmVersion: 'test' });
  return readTar(gunzipSync(readFileSync(join(other.output, result.filename)))).find((entry) => entry.name === 'package/index.d.ts').data;
}
function withDeclarations(declarations) {
  const path = damaged((entries) => { entryNamed(entries, 'index.d.ts').data = declarations; });
  return verify(path, { generator: generatorEmitting(declarations) });
}

test('the consumer typecheck asserts both directions of the contract surface', (t) => {
  assert.deepEqual(contractSurface(fixtureContract()), { paths: ['/tasks', '/tasks/{id}'], operationIds: ['listTasks', 'createTask', 'getTask'] });
  assert.match(consumerSource(fixtureContract()), /Surface<'paths', keyof paths, "\/tasks" \| "\/tasks\/\{id\}">/);
  const failure = (contractOptions) => {
    try { withDeclarations(declarationsFor(t, contractOptions)); } catch (error) { return error.message; }
    return assert.fail('The surface check accepted declarations that differ from the contract');
  };
  // A generator that drops a path and its operation.
  const dropped = failure({ dropTask: true });
  assert.match(dropped, /under bundler resolution/);
  assert.match(dropped, /under bundler resolution:\nconsumer\.ts\(/, 'diagnostics should use paths relative to the consumer');
  assert.match(dropped, /paths: \{ missing: "\/tasks\/\{id\}"; unexpected: never/);
  assert.match(dropped, /operations: \{ missing: "getTask"; unexpected: never/);
  // A generator that invents a path and an operation.
  const invented = failure({ extraPath: true });
  assert.match(invented, /paths: \{ missing: never; unexpected: "\/people"/);
  assert.match(invented, /operations: \{ missing: never; unexpected: "listPeople"/);
  // Operations are checked independently of paths.
  const operationOnly = failure({ extraOperation: true });
  assert.match(operationOnly, /operations: \{ missing: never; unexpected: "deleteTask"/);
  assert.doesNotMatch(operationOnly, /paths: \{/);
});

test('the tar reader reports malformed archives instead of skipping them', () => {
  const archive = gunzipSync(shared.bytes);
  assert.equal(readTar(archive).length, 5);
  const corrupt = Buffer.from(archive);
  corrupt[10] ^= 0xff;
  assert.throws(() => readTar(corrupt), /checksum mismatch/);
  assert.throws(() => readTar(archive.subarray(0, 520)), /truncated/);
  assert.throws(() => readTar(archive.subarray(0, archive.length - 1024)), /no end marker/);
  const trailing = Buffer.concat([archive.subarray(0, archive.length - 1024), Buffer.alloc(1024), Buffer.from('x')]);
  assert.throws(() => readTar(trailing), /Unexpected data after the tar end marker/);
});

test('the verify CLI uses exit status 0, 1 and 2', () => {
  const cli = (...args) => spawnSync(process.execPath, [join(shared.fixture.tool, 'verify.mjs'), ...args], { encoding: 'utf8' });
  const good = cli(join(shared.fixture.output, shared.filename));
  assert.equal(good.status, 0, good.stderr);
  assert.equal(JSON.parse(good.stdout).filename, shared.filename);
  assert.equal(JSON.parse(good.stdout).source_verified, false);
  assert.match(good.stderr, /^api-types: diagnostic verification; the recorded source commit, contract and lockfile were not compared/);
  const release = cli(shared.release.path, '--release', '--version', '0.12.0');
  assert.equal(release.status, 0, release.stderr);
  assert.equal(JSON.parse(release.stdout).source_verified, true);
  assert.equal(release.stderr, '', 'a verified release has nothing to warn about');
  const misattributed = cli(releasePackage(editJson('provenance.json', (value) => { value.backend_commit = '0'.repeat(40); })), '--release', '--version', '0.12.0');
  assert.equal(misattributed.status, 1);
  assert.match(misattributed.stderr, /^api-types: Package records backend commit 0{40}/);
  const bad = cli(damaged((entries) => entries.push({ name: 'package/extra.js', data: Buffer.from('x') })));
  assert.equal(bad.status, 1);
  assert.match(bad.stderr, /^api-types: Unexpected tar entries/);
  for (const args of [[], ['a.tgz', 'b.tgz'], ['a.tgz', '--sha256-file'], ['a.tgz', '--nope'], ['a.tgz', '--release'], ['a.tgz', '--version'], ['a.tgz', '--release', '--release', '--version', '0.12.0']]) {
    const result = cli(...args);
    assert.equal(result.status, 2, args.join(' '));
    assert.match(result.stderr, /usage: node scripts\/api-types\/verify\.mjs/);
  }
  const explicit = damaged((entries) => entries, { sidecar: null });
  writeFileSync(join(shared.directory, 'elsewhere.sha256'), `${sha256(readFileSync(explicit))}  ${shared.filename}\n`);
  assert.equal(cli(explicit, '--sha256-file', join(shared.directory, 'elsewhere.sha256')).status, 0);
});
