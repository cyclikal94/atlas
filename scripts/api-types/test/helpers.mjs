import { execFileSync } from 'node:child_process';
import { chmodSync, copyFileSync, mkdirSync, mkdtempSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { devNull, tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { gzipSync } from 'node:zlib';
import { TAR_MTIME } from '../lib.mjs';

export const realTool = dirname(dirname(fileURLToPath(import.meta.url)));
export const realBackend = dirname(dirname(realTool));
export const pinnedNode = '22.23.2';

const gitEnvironment = {
  ...process.env, GIT_CONFIG_GLOBAL: devNull, GIT_CONFIG_SYSTEM: devNull, GIT_CONFIG_NOSYSTEM: '1',
  GIT_AUTHOR_NAME: 'Fixture', GIT_AUTHOR_EMAIL: 'fixture@example.invalid',
  GIT_COMMITTER_NAME: 'Fixture', GIT_COMMITTER_EMAIL: 'fixture@example.invalid',
};
export const git = (cwd, ...args) => execFileSync('git', args, { cwd, env: gitEnvironment, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] });

const response = (schema) => ({ description: 'Success', content: { 'application/json': { schema } } });

/** A tiny OpenAPI 3.1 document; `padding` adds bulk schemas so a contract can exceed 1 MiB. */
export function fixtureContract({ version = '0.12.0', padding = 0, extraPath = false, extraOperation = false, dropTask = false } = {}) {
  const paths = {
    '/tasks': {
      get: { operationId: 'listTasks', responses: { 200: response({ type: 'array', items: { $ref: '#/components/schemas/Task' } }) } },
      post: { operationId: 'createTask', requestBody: { content: { 'application/json': { schema: { $ref: '#/components/schemas/Task' } } } }, responses: { 201: response({ $ref: '#/components/schemas/Task' }) } },
    },
  };
  if (!dropTask) {
    paths['/tasks/{id}'] = {
      get: {
        operationId: 'getTask', parameters: [{ name: 'id', in: 'path', required: true, schema: { type: 'string' } }],
        responses: { 200: response({ $ref: '#/components/schemas/Task' }) },
      },
    };
  }
  if (extraOperation) paths['/tasks/{id}'].delete = { operationId: 'deleteTask', parameters: paths['/tasks/{id}'].get.parameters, responses: { 204: { description: 'Deleted' } } };
  if (extraPath) paths['/people'] = { get: { operationId: 'listPeople', responses: { 200: response({ type: 'array', items: { type: 'string' } }) } } };
  const schemas = { Task: { type: 'object', required: ['id'], properties: { id: { type: 'string' }, title: { type: 'string' } } } };
  for (let index = 0; index < padding; index += 1) {
    schemas[`Padding${index}`] = { type: 'object', description: `Bulk schema ${index}. ${'x'.repeat(280)}`, properties: { value: { type: 'string' } } };
  }
  return { openapi: '3.1.0', info: { title: 'Fixture', version }, paths, components: { schemas } };
}

export const serialise = (contract) => JSON.stringify(contract, null, 2) + '\n';

/**
 * A committed git repository shaped like the backend: `api/openapi.json`, `LICENSE` and a copy of the
 * tool directory whose `node_modules` links to the real, lock-installed generator.
 */
export function makeRepo(t, { contract = fixtureContract(), licenseMode = 0o644, contractText = serialise(contract) } = {}) {
  const repo = mkdtempSync(join(tmpdir(), 'atlas-fixture-repo-'));
  t.after(() => rmSync(repo, { recursive: true, force: true }));
  mkdirSync(join(repo, 'api'));
  writeFileSync(join(repo, 'api/openapi.json'), contractText);
  writeFileSync(join(repo, 'LICENSE'), 'Fixture licence text\n');
  chmodSync(join(repo, 'LICENSE'), licenseMode);
  const tool = join(repo, 'scripts/api-types');
  mkdirSync(tool, { recursive: true });
  for (const name of ['package.json', 'package-lock.json', '.node-version', 'lib.mjs', 'generate.mjs', 'verify.mjs']) copyFileSync(join(realTool, name), join(tool, name));
  git(repo, 'init', '-q');
  git(repo, 'add', '-A');
  git(repo, '-c', 'commit.gpgsign=false', 'commit', '-q', '-m', 'Fixture');
  symlinkSync(join(realTool, 'node_modules'), join(tool, 'node_modules'));
  return { repo, tool, output: join(repo, 'target/api-types') };
}

/** A second checkout of the same commit at a different path. */
export function cloneRepo(t, fixture) {
  const parent = makeDirectory(t, 'atlas-fixture-clone-');
  const repo = join(parent, 'another', 'checkout');
  mkdirSync(dirname(repo), { recursive: true });
  git(parent, 'clone', '-q', fixture.repo, repo);
  const tool = join(repo, 'scripts/api-types');
  symlinkSync(join(realTool, 'node_modules'), join(tool, 'node_modules'));
  return { repo, tool, output: join(repo, 'target/api-types') };
}

export function makeDirectory(t, prefix = 'atlas-api-types-test-') {
  const directory = mkdtempSync(join(tmpdir(), prefix));
  t.after(() => rmSync(directory, { recursive: true, force: true }));
  return directory;
}

const octal = (value, width) => value.toString(8).padStart(width - 1, '0') + '\0';

/** Writes a ustar archive, so tests can produce deliberately damaged packages. */
export function writeTar(entries) {
  const blocks = [];
  for (const { name, data, mode = 0o644, uid = 0, gid = 0, mtime = TAR_MTIME } of entries) {
    const header = Buffer.alloc(512);
    header.write(name, 0, 100, 'utf8');
    header.write(octal(mode, 8), 100, 'latin1');
    header.write(octal(uid, 8), 108, 'latin1');
    header.write(octal(gid, 8), 116, 'latin1');
    header.write(octal(data.length, 12), 124, 'latin1');
    header.write(octal(mtime, 12), 136, 'latin1');
    header.fill(0x20, 148, 156);
    header.write('0', 156, 'latin1');
    header.write('ustar\0', 257, 'latin1');
    header.write('00', 263, 'latin1');
    let sum = 0;
    for (const byte of header) sum += byte;
    header.write(sum.toString(8).padStart(6, '0') + '\0 ', 148, 'latin1');
    blocks.push(header, data, Buffer.alloc((512 - (data.length % 512)) % 512));
  }
  blocks.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(blocks));
}
