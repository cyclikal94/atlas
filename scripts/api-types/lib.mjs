import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import {
  chmodSync, existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, rmSync, writeFileSync,
} from 'node:fs';
import { tmpdir } from 'node:os';
import { basename, join, resolve } from 'node:path';
import { gunzipSync } from 'node:zlib';

export const PACKAGE_NAME = '@atlas/api-types';
export const PACKAGE_LICENSE = 'AGPL-3.0-only';
export const PACKAGE_FILES = ['LICENSE', 'index.d.ts', 'openapi.json', 'package.json', 'provenance.json'];
export const PACKAGE_JSON_KEYS = ['name', 'version', 'license', 'types', 'exports', 'files'];
export const PROVENANCE_KEYS = ['backend_commit', 'contract_sha256', 'api_version', 'generator', 'generator_lock_sha256'];
export const FORBIDDEN_PACKAGE_KEYS = [
  'dependencies', 'peerDependencies', 'optionalDependencies', 'devDependencies', 'bundledDependencies',
  'scripts', 'main', 'module', 'bin',
];
// npm packs every entry with this fixed modification time (1985-10-26T08:15:00Z).
export const TAR_MTIME = 499162500;
// Longest release tag the Release workflow accepts is 120 characters including the leading "v".
export const MAX_VERSION_LENGTH = 119;
// A contract is read whole from git; the default 1 MiB child-process buffer is a latent cliff.
const GIT_MAX_BUFFER = 512 * 1024 * 1024;
// Keep the caller's npm configuration (registries, pack defaults, notices) out of the archive.
const npmEnvironment = (root) => ({
  npm_config_userconfig: join(root, 'absent-user.npmrc'),
  npm_config_globalconfig: join(root, 'absent-global.npmrc'),
  npm_config_update_notifier: 'false',
  npm_config_audit: 'false',
  npm_config_fund: 'false',
});

/** A failed release or verification check (CLI exit status 1). */
export class ApiTypesError extends Error {}
/** Invalid command-line use (CLI exit status 2). */
export class UsageError extends Error {}

export const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');

// Mirrors the tag grammar in the Release workflow's metadata job; the workflow test executes
// that job's script against the same candidates so the two cannot drift silently.
const NUMBER = '(?:0|[1-9][0-9]*)';
const IDENTIFIER = `(?:${NUMBER}|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)`;
const SEMVER = new RegExp(`^(${NUMBER})\\.(${NUMBER})\\.(${NUMBER})(?:-(${IDENTIFIER}(?:\\.${IDENTIFIER})*))?$`);

/** Strict SemVer 2.0.0 without build metadata. Returns null when the text is not accepted. */
export function parseSemver(text) {
  if (typeof text !== 'string') return null;
  const match = SEMVER.exec(text);
  if (!match) return null;
  return { major: match[1], minor: match[2], patch: match[3], prerelease: match[4] ?? null, core: `${match[1]}.${match[2]}.${match[3]}` };
}

/** A diagnostic package is versioned `0.0.0-<label>`, which no release tag can be. */
const isDiagnosticVersion = (semver) => semver.core === '0.0.0' && semver.prerelease !== null;

/** The full release-version grammar the Release workflow accepts, without its leading "v". */
export const isAcceptedVersion = (text) => parseSemver(text) !== null && text.length <= MAX_VERSION_LENGTH;

/**
 * The version rules that need no toolchain: `apiVersion` is the contract's `info.version`. A release
 * version carries that version's `MAJOR.MINOR.PATCH` (prereleases included); anything else is a
 * `0.0.0-<label>` diagnostic. Shared by generation and by release verification.
 */
export function assertVersionPolicy({ version, release, apiVersion }) {
  const semver = parseSemver(version);
  if (!semver) throw new ApiTypesError(`--version "${version}" is not MAJOR.MINOR.PATCH SemVer (optional prerelease, no build metadata)`);
  if (!isAcceptedVersion(version)) throw new ApiTypesError(`--version is longer than ${MAX_VERSION_LENGTH} characters`);
  if (!parseSemver(apiVersion)) throw new ApiTypesError(`Contract info.version "${apiVersion}" is missing or not strict SemVer`);
  if (!release) {
    if (!isDiagnosticVersion(semver)) {
      throw new ApiTypesError(`Non-release builds must use 0.0.0-<label>; got ${version}. Pass --release only from a clean tagged release build`);
    }
    return semver;
  }
  if (semver.core !== apiVersion) {
    throw new ApiTypesError(`Release version ${version} carries core ${semver.core}, but the contract's info.version is ${apiVersion}; a release version must carry the contract's version (align them before tagging)`);
  }
  return semver;
}

/**
 * Version policy for generation: the rules above, plus, for release builds, that `nodeVersion` is
 * the pinned `pinnedNode` (the gzip bytes depend on the Node build's zlib).
 */
export function assertPolicy({ version, release, apiVersion, nodeVersion, pinnedNode }) {
  const semver = assertVersionPolicy({ version, release, apiVersion });
  if (release && nodeVersion !== pinnedNode) {
    throw new ApiTypesError(`Release builds require Node ${pinnedNode} (scripts/api-types/.node-version); running ${nodeVersion}`);
  }
  return semver;
}

function git(repo, args) {
  try {
    return execFileSync('git', args, { cwd: repo, maxBuffer: GIT_MAX_BUFFER, stdio: ['ignore', 'pipe', 'pipe'] });
  } catch (error) {
    throw new ApiTypesError(`git ${args[0]} failed: ${String(error.stderr ?? error.message).trim()}`);
  }
}

/** Reads the contract as committed at HEAD and refuses a working copy that differs. */
export function readContract(repo) {
  const commit = git(repo, ['rev-parse', 'HEAD']).toString('utf8').trim();
  if (!/^[0-9a-f]{40}$/.test(commit)) throw new ApiTypesError(`Unexpected git revision "${commit}"`);
  const schema = git(repo, ['show', `${commit}:api/openapi.json`]);
  const working = join(repo, 'api/openapi.json');
  if (!existsSync(working) || !schema.equals(readFileSync(working))) {
    throw new ApiTypesError('Working contract differs from recorded commit; review/commit the contract before generating');
  }
  let document;
  try {
    document = JSON.parse(schema.toString('utf8'));
  } catch (error) {
    throw new ApiTypesError(`api/openapi.json is not valid JSON: ${error.message}`);
  }
  return { commit, schema, apiVersion: document?.info?.version };
}

/** Tracked files must match HEAD, so provenance cannot describe inputs that were never committed. */
export function assertCleanTrackedTree(repo) {
  const dirty = git(repo, ['status', '--porcelain', '--untracked-files=no']).toString('utf8').trim();
  if (dirty) throw new ApiTypesError(`Release builds need a clean tracked working tree; uncommitted changes:\n${dirty}`);
}

/** The exact generator pin from package.json, which must also be the version installed. */
export function readGeneratorPin(tool) {
  const pinned = JSON.parse(readFileSync(join(tool, 'package.json'), 'utf8')).devDependencies?.['openapi-typescript'];
  if (!/^\d+\.\d+\.\d+$/.test(pinned ?? '')) throw new ApiTypesError(`openapi-typescript must be pinned to an exact version in package.json; found "${pinned}"`);
  const installedFile = join(tool, 'node_modules/openapi-typescript/package.json');
  if (!existsSync(installedFile)) throw new ApiTypesError('openapi-typescript is not installed; run npm ci --ignore-scripts in scripts/api-types');
  const installed = JSON.parse(readFileSync(installedFile, 'utf8')).version;
  if (installed !== pinned) throw new ApiTypesError(`Installed openapi-typescript ${installed} differs from the ${pinned} pin; run npm ci --ignore-scripts`);
  return `openapi-typescript@${pinned}`;
}

/** Runs the pinned generator on `input`, writing declarations to `output`. */
export function pinnedGenerator(tool) {
  const cli = join(tool, 'node_modules/openapi-typescript/bin/cli.js');
  return (input, output) => {
    try {
      execFileSync(process.execPath, [cli, input, '-o', output], { cwd: tool, stdio: ['ignore', 'pipe', 'pipe'] });
    } catch (error) {
      throw new ApiTypesError(`openapi-typescript failed: ${String(error.stderr ?? error.message).trim()}`);
    }
  };
}

/** Generates twice on identical bytes and refuses any difference. */
export function generateDeclarations(generator, schema, workdir) {
  const input = join(workdir, 'openapi.json');
  writeFileSync(input, schema);
  const outputs = ['first.d.ts', 'second.d.ts'].map((name) => {
    const output = join(workdir, name);
    generator(input, output);
    return readFileSync(output);
  });
  if (!outputs[0].equals(outputs[1])) throw new ApiTypesError('Non-deterministic API declarations');
  return outputs[0];
}

export function withUmask(mask, action) {
  const previous = process.umask(mask);
  try { return action(); } finally { process.umask(previous); }
}

function writePackageFile(directory, name, bytes) {
  const path = join(directory, name);
  writeFileSync(path, bytes);
  // npm records file modes, so never let the ambient umask or the source file's mode decide them.
  chmodSync(path, 0o644);
}

export function packageManifest(version) {
  return {
    name: PACKAGE_NAME, version, license: PACKAGE_LICENSE, types: './index.d.ts',
    exports: { '.': { types: './index.d.ts' }, './openapi.json': './openapi.json', './provenance.json': './provenance.json' },
    files: ['index.d.ts', 'openapi.json', 'provenance.json', 'LICENSE'],
  };
}

/** Assembles the package in `root` and packs it with npm; returns the tarball name and bytes. */
export function buildPackage({ root, version, declarations, schema, license, provenance }) {
  const directory = join(root, 'package');
  const destination = join(root, 'packed');
  mkdirSync(directory, { recursive: true });
  mkdirSync(destination, { recursive: true });
  writePackageFile(directory, 'index.d.ts', declarations);
  writePackageFile(directory, 'openapi.json', schema);
  writePackageFile(directory, 'LICENSE', license);
  writePackageFile(directory, 'provenance.json', JSON.stringify(provenance, null, 2) + '\n');
  writePackageFile(directory, 'package.json', JSON.stringify(packageManifest(version), null, 2) + '\n');
  let packed;
  try {
    packed = JSON.parse(execFileSync('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', destination],
      { cwd: directory, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], env: { ...process.env, ...npmEnvironment(root) } }));
  } catch (error) {
    throw new ApiTypesError(`npm pack failed: ${String(error.stderr ?? error.message).trim()}`);
  }
  const filename = packed[0].filename;
  return { filename, bytes: readFileSync(join(destination, filename)) };
}

/** SHA-256 of the decompressed tar stream: comparable across Node/zlib builds, unlike the gzip bytes. */
export const payloadSha256 = (tarball) => sha256(gunzipSync(tarball));

function octal(field, label) {
  const text = field.toString('latin1').replace(/[\0 ]+$/, '').replace(/^[\0 ]+/, '');
  if (field[0] & 0x80) throw new ApiTypesError(`Unsupported base-256 tar field: ${label}`);
  if (text === '') return 0;
  if (!/^[0-7]+$/.test(text)) throw new ApiTypesError(`Malformed tar field: ${label}`);
  return parseInt(text, 8);
}

const cString = (field) => field.toString('utf8').replace(/\0.*$/s, '');

/** Minimal ustar reader; anything other than plain files (including pax headers) is reported, not skipped. */
export function readTar(archive) {
  const entries = [];
  let offset = 0;
  while (offset + 512 <= archive.length) {
    const header = archive.subarray(offset, offset + 512);
    if (header.every((byte) => byte === 0)) {
      if (!archive.subarray(offset).every((byte) => byte === 0)) throw new ApiTypesError('Unexpected data after the tar end marker');
      return entries;
    }
    const expected = octal(header.subarray(148, 156), 'checksum');
    let actual = 0;
    for (let i = 0; i < 512; i += 1) actual += i >= 148 && i < 156 ? 32 : header[i];
    if (actual !== expected) throw new ApiTypesError('Tar header checksum mismatch');
    const prefix = cString(header.subarray(345, 500));
    const name = (prefix ? `${prefix}/` : '') + cString(header.subarray(0, 100));
    const size = octal(header.subarray(124, 136), 'size');
    const type = String.fromCharCode(header[156] || 0x30);
    const start = offset + 512;
    if (start + size > archive.length) throw new ApiTypesError(`Tar entry ${name} is truncated`);
    entries.push({
      name, type, size, data: archive.subarray(start, start + size),
      mode: octal(header.subarray(100, 108), 'mode'), uid: octal(header.subarray(108, 116), 'uid'),
      gid: octal(header.subarray(116, 124), 'gid'), mtime: octal(header.subarray(136, 148), 'mtime'),
    });
    offset = start + Math.ceil(size / 512) * 512;
  }
  throw new ApiTypesError('Tar archive has no end marker');
}

const exactKeys = (object, keys) => JSON.stringify(Object.keys(object).sort()) === JSON.stringify([...keys].sort());

function parseJson(entry) {
  try {
    return JSON.parse(entry.data.toString('utf8'));
  } catch (error) {
    throw new ApiTypesError(`${entry.name} is not valid JSON: ${error.message}`);
  }
}

/**
 * Structural checks that need no generator or compiler: entry allowlist and metadata, manifest and
 * provenance shape, and the internal consistency of the contract, hash and version.
 */
export function inspectTarball(tarball, expectedFilename) {
  let entries;
  try {
    entries = readTar(gunzipSync(tarball));
  } catch (error) {
    if (error instanceof ApiTypesError) throw error;
    throw new ApiTypesError(`Not a gzip-compressed tar archive: ${error.message}`);
  }
  const byName = new Map();
  for (const entry of entries) {
    if (byName.has(entry.name)) throw new ApiTypesError(`Duplicate tar entry ${entry.name}`);
    byName.set(entry.name, entry);
  }
  const expected = PACKAGE_FILES.map((name) => `package/${name}`);
  const unexpected = [...byName.keys()].filter((name) => !expected.includes(name));
  if (unexpected.length) throw new ApiTypesError(`Unexpected tar entries: ${unexpected.join(', ')}`);
  const missing = expected.filter((name) => !byName.has(name));
  if (missing.length) throw new ApiTypesError(`Missing tar entries: ${missing.join(', ')}`);
  for (const entry of entries) {
    if (entry.type !== '0') throw new ApiTypesError(`${entry.name} is not a regular file (type ${entry.type})`);
    if (entry.mode !== 0o644) throw new ApiTypesError(`${entry.name} has mode ${entry.mode.toString(8).padStart(4, '0')}, expected 0644`);
    if (entry.uid !== 0 || entry.gid !== 0) throw new ApiTypesError(`${entry.name} has owner ${entry.uid}:${entry.gid}, expected 0:0`);
    if (entry.mtime !== TAR_MTIME) throw new ApiTypesError(`${entry.name} has modification time ${entry.mtime}, expected ${TAR_MTIME}`);
  }

  const manifest = parseJson(byName.get('package/package.json'));
  const forbidden = FORBIDDEN_PACKAGE_KEYS.filter((key) => key in manifest);
  if (forbidden.length) throw new ApiTypesError(`package.json has forbidden keys: ${forbidden.join(', ')}`);
  if (!exactKeys(manifest, PACKAGE_JSON_KEYS)) throw new ApiTypesError(`package.json keys must be exactly ${PACKAGE_JSON_KEYS.join(', ')}`);
  if (manifest.name !== PACKAGE_NAME) throw new ApiTypesError(`package.json name is ${manifest.name}, expected ${PACKAGE_NAME}`);
  if (!parseSemver(manifest.version) || manifest.version.length > MAX_VERSION_LENGTH) throw new ApiTypesError(`package.json version "${manifest.version}" is not accepted release SemVer`);
  if (JSON.stringify({ ...manifest, version: '0.0.0' }) !== JSON.stringify(packageManifest('0.0.0'))) {
    throw new ApiTypesError('package.json does not match the declarations-only manifest');
  }
  const filename = `atlas-api-types-${manifest.version}.tgz`;
  if (expectedFilename !== undefined && expectedFilename !== filename) {
    throw new ApiTypesError(`Asset is named ${expectedFilename} but package.json version ${manifest.version} requires ${filename}`);
  }

  const provenance = parseJson(byName.get('package/provenance.json'));
  if (!exactKeys(provenance, PROVENANCE_KEYS)) throw new ApiTypesError(`provenance.json keys must be exactly ${PROVENANCE_KEYS.join(', ')}`);
  if (!/^[0-9a-f]{40}$/.test(provenance.backend_commit)) throw new ApiTypesError('provenance.json backend_commit is not a 40-character commit');
  if (!/^[0-9a-f]{64}$/.test(provenance.generator_lock_sha256)) throw new ApiTypesError('provenance.json generator_lock_sha256 is not a SHA-256');
  if (!/^openapi-typescript@\d+\.\d+\.\d+$/.test(provenance.generator)) throw new ApiTypesError(`provenance.json generator "${provenance.generator}" is not an exact pin`);
  const schema = byName.get('package/openapi.json');
  if (provenance.contract_sha256 !== sha256(schema.data)) throw new ApiTypesError('provenance.json contract_sha256 does not match the packaged openapi.json');
  const apiVersion = parseJson(schema)?.info?.version;
  if (!parseSemver(apiVersion)) throw new ApiTypesError(`Packaged openapi.json info.version "${apiVersion}" is not strict SemVer`);
  if (provenance.api_version !== apiVersion) {
    throw new ApiTypesError(`provenance.json api_version ${provenance.api_version} differs from the packaged contract's ${apiVersion}`);
  }
  return { entries: byName, manifest, provenance, filename, contract: parseJson(schema) };
}

/** The generation pipeline. Nothing is written to `output` unless every check has passed. */
export function generate({ repo, tool, version, output, release = false, generator, builder = buildPackage, nodeVersion = process.versions.node, npmVersion }) {
  const { commit, schema, apiVersion } = readContract(repo);
  const pinnedNode = readFileSync(join(tool, '.node-version'), 'utf8').trim();
  assertPolicy({ version, release, apiVersion, nodeVersion, pinnedNode });
  if (release) assertCleanTrackedTree(repo);
  const generatorName = readGeneratorPin(tool);
  const provenance = {
    backend_commit: commit, contract_sha256: sha256(schema), api_version: apiVersion,
    generator: generatorName, generator_lock_sha256: sha256(readFileSync(join(tool, 'package-lock.json'))),
  };
  const license = readFileSync(join(repo, 'LICENSE'));
  const workdir = mkdtempSync(join(tmpdir(), 'atlas-api-types-'));
  try {
    const declarations = generateDeclarations(generator ?? pinnedGenerator(tool), schema, workdir);
    // Two independent builds under opposite umasks and in different directories: any leak of ambient
    // file modes, paths or time into the archive makes them differ.
    const builds = [0o022, 0o077].map((mask, index) => withUmask(mask, () => builder({
      root: join(workdir, `build-${index}`), version, declarations, schema, license, provenance,
    })));
    if (builds[0].filename !== builds[1].filename || !builds[0].bytes.equals(builds[1].bytes)) throw new ApiTypesError('Non-deterministic API package');
    const { filename, bytes } = builds[0];
    inspectTarball(bytes, filename);
    if (filename !== `atlas-api-types-${version}.tgz`) throw new ApiTypesError(`npm produced ${filename}, expected atlas-api-types-${version}.tgz`);
    const checksum = sha256(bytes);
    mkdirSync(output, { recursive: true });
    for (const [name, content] of [[filename, bytes], [`${filename}.sha256`, `${checksum}  ${filename}\n`]]) {
      const temporary = join(output, `.${name}.partial`);
      writeFileSync(temporary, content);
      renameSync(temporary, join(output, name));
    }
    return { filename, sha256: checksum, tar_payload_sha256: payloadSha256(bytes), version, ...provenance, node: nodeVersion, npm: npmVersion };
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
}

/** `sha256sum -c` format: `<64 lowercase hex>` two spaces, the file name, and a newline. */
export function parseSidecar(text) {
  const match = /^([0-9a-f]{64})  ([^\n/]+)\n$/.exec(text);
  if (!match) throw new ApiTypesError('Checksum sidecar must be exactly "<64 lowercase hex>  <file name>" and a newline');
  return { checksum: match[1], filename: match[2] };
}

const HTTP_METHODS = ['get', 'put', 'post', 'delete', 'options', 'head', 'patch', 'trace'];

/** Every path and every operationId in the contract, derived so the check needs no maintenance. */
export function contractSurface(contract) {
  const paths = Object.keys(contract.paths ?? {});
  const operationIds = [];
  for (const [path, item] of Object.entries(contract.paths ?? {})) {
    if (item && typeof item === 'object' && '$ref' in item) throw new ApiTypesError(`Path item ${path} uses $ref, which the surface check cannot enumerate`);
    for (const method of HTTP_METHODS) {
      const id = item?.[method]?.operationId;
      if (id !== undefined) operationIds.push(id);
    }
  }
  return { paths, operationIds };
}

const union = (names) => (names.length ? names.map((name) => JSON.stringify(name)).join(' | ') : 'never');

/** A consumer module asserting, in both directions, that the declarations expose exactly the contract's surface. */
export function consumerSource(contract) {
  const { paths, operationIds } = contractSurface(contract);
  return [
    "import type { operations, paths } from '@atlas/api-types';",
    '// Resolves to `true` when the two key sets are equal; otherwise its type names what is missing or unexpected.',
    'type Surface<Label extends string, Actual extends PropertyKey, Expected extends PropertyKey> =',
    '  [Exclude<Expected, Actual>, Exclude<Actual, Expected>] extends [never, never]',
    '    ? true',
    '    : { [K in Label]: { missing: Exclude<Expected, Actual>; unexpected: Exclude<Actual, Expected> } };',
    `export const pathsMatchContract: Surface<'paths', keyof paths, ${union(paths)}> = true;`,
    `export const operationsMatchContract: Surface<'operations', keyof operations, ${union(operationIds)}> = true;`,
    '',
  ].join('\n');
}

const RESOLUTIONS = { bundler: { module: 'ESNext', moduleResolution: 'bundler' }, node16: { module: 'node16', moduleResolution: 'node16' } };

/** Installs the package into a scratch consumer and compiles it strictly under each resolution mode. */
export function typecheckConsumer({ tool, entries, contract, workdir }) {
  const tsc = join(tool, 'node_modules/typescript/bin/tsc');
  if (!existsSync(tsc)) throw new ApiTypesError('typescript is not installed; run npm ci --ignore-scripts in scripts/api-types');
  const consumer = join(workdir, 'consumer');
  const installed = join(consumer, 'node_modules/@atlas/api-types');
  mkdirSync(installed, { recursive: true });
  for (const [name, entry] of entries) writeFileSync(join(installed, name.replace(/^package\//, '')), entry.data);
  writeFileSync(join(consumer, 'package.json'), '{"private":true,"type":"module"}\n');
  writeFileSync(join(consumer, 'consumer.ts'), consumerSource(contract));
  for (const [label, resolution] of Object.entries(RESOLUTIONS)) {
    writeFileSync(join(consumer, `tsconfig.${label}.json`), JSON.stringify({
      compilerOptions: {
        strict: true, exactOptionalPropertyTypes: true, noUncheckedIndexedAccess: true, skipLibCheck: false,
        noEmit: true, target: 'ES2022', types: [], ...resolution,
      },
      files: ['consumer.ts'],
    }));
    try {
      execFileSync(process.execPath, [tsc, '-p', `tsconfig.${label}.json`], { cwd: consumer, stdio: ['ignore', 'pipe', 'pipe'], encoding: 'utf8' });
    } catch (error) {
      throw new ApiTypesError(`Consumer typecheck failed under ${label} resolution:\n${String(error.stdout ?? '').trim() || String(error.stderr ?? error.message).trim()}`);
    }
  }
}

/**
 * Ties a release package to the trusted checkout it is verified from (the documented recipe checks out
 * the release tag first): the recorded source commit, the committed contract bytes, the generator
 * lockfile digest and the release version must all be the checkout's own. A matching sidecar proves the
 * bytes are unchanged since publication; only this connects them to the tagged source.
 */
export function verifySource({ repo, tool, entries, manifest, provenance }) {
  const { commit, schema, apiVersion } = readContract(repo);
  assertVersionPolicy({ version: manifest.version, release: true, apiVersion });
  assertCleanTrackedTree(repo);
  if (provenance.backend_commit !== commit) {
    throw new ApiTypesError(`Package records backend commit ${provenance.backend_commit}, but this checkout is at ${commit}; check out the release tag before verifying`);
  }
  if (!schema.equals(entries.get('package/openapi.json').data)) {
    throw new ApiTypesError(`Packaged openapi.json differs from api/openapi.json at ${commit}`);
  }
  const lock = sha256(readFileSync(join(tool, 'package-lock.json')));
  if (provenance.generator_lock_sha256 !== lock) {
    throw new ApiTypesError(`Package records generator lock digest ${provenance.generator_lock_sha256}, but this checkout's scripts/api-types/package-lock.json is ${lock}`);
  }
}

/**
 * Independent checks of a produced asset: transport integrity, contents, and the declarations themselves.
 * With `release`, `version` (the expected release version, from the tag) is required and the package is also
 * compared with the trusted checkout `repo`; without it the package is a diagnostic and its recorded source
 * is reported as unverified rather than assumed.
 */
export function verifyAsset({ tarballPath, sidecarPath = `${tarballPath}.sha256`, tool, repo = resolve(tool, '../..'), generator, version, release = false }) {
  if (release && version === undefined) throw new UsageError('--release needs the expected release --version');
  if (!existsSync(tarballPath)) throw new ApiTypesError(`Tarball not found: ${tarballPath}`);
  if (!existsSync(sidecarPath)) throw new ApiTypesError(`Checksum sidecar not found: ${sidecarPath}`);
  const bytes = readFileSync(tarballPath);
  const name = basename(tarballPath);
  const sidecar = parseSidecar(readFileSync(sidecarPath, 'utf8'));
  if (sidecar.filename !== name) throw new ApiTypesError(`Checksum sidecar names ${sidecar.filename}, not ${name}`);
  if (sidecar.checksum !== sha256(bytes)) throw new ApiTypesError(`Tarball SHA-256 ${sha256(bytes)} does not match its sidecar (${sidecar.checksum})`);
  const { entries, manifest, provenance, contract } = inspectTarball(bytes, name);
  if (version !== undefined && manifest.version !== version) {
    throw new ApiTypesError(`Package version ${manifest.version} differs from the expected version ${version}`);
  }
  if (release) {
    verifySource({ repo, tool, entries, manifest, provenance });
  } else if (!isDiagnosticVersion(parseSemver(manifest.version))) {
    throw new ApiTypesError(`Package version ${manifest.version} is a release version; verify it with --release --version ${manifest.version} from its tagged checkout so its provenance is checked`);
  }
  const pinned = readGeneratorPin(tool);
  if (provenance.generator !== pinned) {
    throw new ApiTypesError(`Package was generated with ${provenance.generator}, but this checkout installs ${pinned}; verify with the matching generator`);
  }
  const workdir = mkdtempSync(join(tmpdir(), 'atlas-api-types-verify-'));
  try {
    const fresh = generateDeclarations(generator ?? pinnedGenerator(tool), entries.get('package/openapi.json').data, workdir);
    if (!fresh.equals(entries.get('package/index.d.ts').data)) {
      throw new ApiTypesError('index.d.ts does not match a fresh generator run on the packaged openapi.json');
    }
    typecheckConsumer({ tool, entries, contract, workdir });
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
  return { filename: name, sha256: sidecar.checksum, tar_payload_sha256: payloadSha256(bytes), version: manifest.version, ...provenance, source_verified: release };
}

/** Strict argument parser: unknown, repeated or value-less options are usage errors. */
export function parseArguments(argv, { values = [], flags = [], positional = 0 }) {
  const options = {};
  const rest = [];
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (values.includes(argument)) {
      const value = argv[index + 1];
      if (value === undefined || value.startsWith('--')) throw new UsageError(`${argument} needs a value`);
      if (argument in options) throw new UsageError(`${argument} was given more than once`);
      options[argument] = value;
      index += 1;
    } else if (flags.includes(argument)) {
      if (argument in options) throw new UsageError(`${argument} was given more than once`);
      options[argument] = true;
    } else if (argument.startsWith('--')) {
      throw new UsageError(`Unknown option ${argument}`);
    } else {
      rest.push(argument);
    }
  }
  if (rest.length !== positional) throw new UsageError(`Expected ${positional} positional argument${positional === 1 ? '' : 's'}, got ${rest.length}`);
  return { options, positional: rest };
}

/** Runs a CLI action with the documented exit statuses: 0 success, 1 failed check, 2 usage error. */
export function runCli(usage, action) {
  try {
    action();
  } catch (error) {
    if (error instanceof UsageError) {
      console.error(`api-types: ${error.message}\n${usage}`);
      process.exitCode = 2;
    } else {
      console.error(`api-types: ${error instanceof ApiTypesError ? error.message : error.stack ?? error}`);
      process.exitCode = 1;
    }
  }
}
