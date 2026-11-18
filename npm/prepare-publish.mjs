import { createHash } from 'node:crypto';
import { access, chmod, copyFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { parseArgs } from 'node:util';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

// Every binary ships inside the rom-cli tarball under bin/native/<os>-<cpu>/,
// so installing never downloads anything and needs no install script.
// macOS is not in this release: its runners were not available. Add a
// darwin entry here and a build job in release.yml when that changes.
export const platforms = [
  {
    os: 'win32',
    cpu: 'x64',
    binary: 'rom.exe',
    artifact: 'rom-win32-x64',
    asset: 'rom-win32-x64.exe',
  },
  {
    os: 'linux',
    cpu: 'x64',
    binary: 'rom',
    artifact: 'rom-linux-x64',
    asset: 'rom-linux-x64',
  },
  {
    os: 'linux',
    cpu: 'arm64',
    binary: 'rom',
    artifact: 'rom-linux-arm64',
    asset: 'rom-linux-arm64',
  },
];

export async function pack({ version, artifacts, out }) {
  if (!/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error(`version must be x.y.z, got ${version}`);
  }

  const template = JSON.parse(
    await readFile(path.join(root, 'npm', 'rom-cli', 'package.json'), 'utf8'),
  );

  await mkdir(out, { recursive: true });
  const assets = path.join(out, 'assets');
  await mkdir(assets, { recursive: true });

  const cliDir = path.join(out, 'rom-cli');
  const binDir = path.join(cliDir, 'bin');
  const sums = [];
  for (const platform of platforms) {
    const source = path.join(artifacts, platform.artifact, platform.binary);
    await access(source);
    const bytes = await readFile(source);
    const hash = createHash('sha256').update(bytes).digest('hex');
    sums.push(`${hash}  ${platform.asset}`);

    // Copy for the GitHub Release, so people can download a binary directly.
    const assetPath = path.join(assets, platform.asset);
    await writeFile(assetPath, bytes);

    // Copy for the npm tarball, in the folder the wrapper looks in.
    const nativeDir = path.join(binDir, 'native', `${platform.os}-${platform.cpu}`);
    await mkdir(nativeDir, { recursive: true });
    const nativePath = path.join(nativeDir, platform.binary);
    await writeFile(nativePath, bytes);

    if (platform.os !== 'win32') {
      await chmod(assetPath, 0o755);
      await chmod(nativePath, 0o755);
    }
  }

  sums.sort();
  await writeFile(path.join(assets, 'SHA256SUMS'), `${sums.join('\n')}\n`);

  await mkdir(binDir, { recursive: true });
  const wrapper = path.join(binDir, 'rom.js');
  await copyFile(path.join(root, 'npm', 'rom-cli', 'bin', 'rom.js'), wrapper);
  await chmod(wrapper, 0o755);
  await copyFile(path.join(root, 'README.md'), path.join(cliDir, 'README.md'));
  await writeLicenses(cliDir);

  const cli = { ...template, version };
  delete cli.optionalDependencies;
  delete cli.scripts;
  await writeFile(path.join(cliDir, 'package.json'), `${JSON.stringify(cli, null, 2)}\n`);
}

async function writeLicenses(dir) {
  await copyFile(path.join(root, 'LICENSE-MIT'), path.join(dir, 'LICENSE-MIT'));
  await copyFile(path.join(root, 'LICENSE-APACHE'), path.join(dir, 'LICENSE-APACHE'));
}

function readArgs(argv) {
  const { values } = parseArgs({
    args: argv,
    options: {
      version: { type: 'string' },
      artifacts: { type: 'string' },
      out: { type: 'string' },
    },
  });
  if (!values.version || !values.artifacts || !values.out) {
    throw new Error('usage: node npm/prepare-publish.mjs --version x.y.z --artifacts DIR --out DIR');
  }
  return values;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const values = readArgs(process.argv.slice(2));
  await pack(values);
}
