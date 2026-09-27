import { execFileSync } from 'node:child_process';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { UsageError, generate, parseArguments, runCli } from './lib.mjs';

const tool = dirname(fileURLToPath(import.meta.url));
const backend = resolve(tool, '../..');
const usage = 'usage: node scripts/api-types/generate.mjs --version <semver> [--output <dir>] [--release]';

runCli(usage, () => {
  const { options } = parseArguments(process.argv.slice(2), { values: ['--version', '--output'], flags: ['--release'] });
  if (!options['--version']) throw new UsageError('--version is required');
  const npmVersion = execFileSync('npm', ['--version'], { encoding: 'utf8' }).trim();
  const result = generate({
    repo: backend, tool, version: options['--version'], release: options['--release'] === true, npmVersion,
    output: resolve(options['--output'] ?? join(backend, 'target/api-types')),
  });
  console.log(JSON.stringify(result));
});
