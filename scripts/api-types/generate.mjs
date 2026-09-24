import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, writeFileSync, mkdirSync, copyFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, resolve, join } from 'node:path';
import { fileURLToPath } from 'node:url';
const here = dirname(fileURLToPath(import.meta.url));
const backend = resolve(here, '../..');
const args = process.argv.slice(2);
const option = (name) => { const i = args.indexOf(name); return i < 0 ? undefined : args[i + 1]; };
const version = option('--version');
const output = resolve(option('--output') ?? join(backend, 'target/api-types'));
if (!version || !/^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$/.test(version)) throw Error('Supply --version with a release or explicit bootstrap SemVer');
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
const commit = execFileSync('git', ['rev-parse', 'HEAD'], { cwd: backend, encoding: 'utf8' }).trim();
const schema = execFileSync('git', ['show', `${commit}:api/openapi.json`], { cwd: backend });
if (!schema.equals(readFileSync(join(backend, 'api/openapi.json')))) throw Error('Working contract differs from recorded commit; review/commit the contract before generating');
const info = JSON.parse(schema).info;
const temporary = mkdtempSync(join(tmpdir(), 'atlas-api-types-'));
try {
  const pkg = join(temporary, 'package'); mkdirSync(pkg);
  const input = join(temporary, 'openapi.json'); writeFileSync(input, schema);
  const generator = join(here, 'node_modules/openapi-typescript/bin/cli.js');
  for (const name of ['first.d.ts', 'second.d.ts']) {
    execFileSync(process.execPath, [generator, input, '-o', join(temporary, name)], { cwd: here, stdio: 'inherit' });
  }
  const declarations = readFileSync(join(temporary, 'first.d.ts'));
  if (!declarations.equals(readFileSync(join(temporary, 'second.d.ts')))) throw Error('Non-deterministic API declarations');
  writeFileSync(join(pkg, 'index.d.ts'), declarations);
  writeFileSync(join(pkg, 'openapi.json'), schema);
  copyFileSync(join(backend, 'LICENSE'), join(pkg, 'LICENSE'));
  const provenance = { backend_commit: commit, contract_sha256: sha256(schema), api_version: info.version,
    generator: 'openapi-typescript@7.4.3', generator_lock_sha256: sha256(readFileSync(join(here, 'package-lock.json'))) };
  writeFileSync(join(pkg, 'provenance.json'), JSON.stringify(provenance, null, 2) + '\n');
  writeFileSync(join(pkg, 'package.json'), JSON.stringify({ name: '@atlas/api-types', version, license: 'AGPL-3.0-only', types: './index.d.ts',
    exports: { '.': { types: './index.d.ts' }, './openapi.json': './openapi.json', './provenance.json': './provenance.json' },
    files: ['index.d.ts', 'openapi.json', 'provenance.json', 'LICENSE'] }, null, 2) + '\n');
  mkdirSync(output, { recursive: true });
  const packed = JSON.parse(execFileSync('npm', ['pack', '--ignore-scripts', '--json', '--pack-destination', output], { cwd: pkg, encoding: 'utf8' }));
  const filename = packed[0].filename;
  const checksum = sha256(readFileSync(join(output, filename)));
  writeFileSync(join(output, filename + '.sha256'), checksum + '  ' + filename + '\n');
  console.log(JSON.stringify({ filename, sha256: checksum, version, ...provenance }));
} finally { rmSync(temporary, { recursive: true, force: true }); }
