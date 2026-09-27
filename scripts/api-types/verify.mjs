import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { UsageError, parseArguments, runCli, verifyAsset } from './lib.mjs';

const tool = dirname(fileURLToPath(import.meta.url));
const usage = 'usage: node scripts/api-types/verify.mjs <tarball> [--sha256-file <path>] [--version <semver>] [--release]\n'
  + '  --release --version <semver> also proves the package came from this checkout (run it at the release tag)';

runCli(usage, () => {
  const { options, positional } = parseArguments(process.argv.slice(2), { values: ['--sha256-file', '--version'], flags: ['--release'], positional: 1 });
  const release = options['--release'] === true;
  if (release && !options['--version']) throw new UsageError('--release needs the expected release --version');
  const sidecar = options['--sha256-file'];
  const result = verifyAsset({
    tarballPath: resolve(positional[0]), release, version: options['--version'], ...(sidecar ? { sidecarPath: resolve(sidecar) } : {}), tool,
  });
  if (!result.source_verified) {
    console.error('api-types: diagnostic verification; the recorded source commit, contract and lockfile were not compared with this checkout (use --release --version <semver> at the release tag)');
  }
  console.log(JSON.stringify(result));
});
