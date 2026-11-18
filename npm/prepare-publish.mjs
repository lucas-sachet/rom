import { createHash } from 'node:crypto';
import { access, chmod, copyFile, mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { parseArgs } from 'node:util';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');

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
  {
    os: 'darwin',
    cpu: 'x64',
    binary: 'rom',
    artifact: 'rom-darwin-x64',
    asset: 'rom-darwin-x64',
  },
  {
    os: 'darwin',
    cpu: 'arm64',
    binary: 'rom',
    artifact: 'rom-darwin-arm64',
    asset: 'rom-darwin-arm64',
  },
];

const repository = {
  type: 'git',
  url: 'git+https://github.com/lucas-sachet/rom.git',
};

export async function pack({ version, artifacts, out }) {
  if (!/^\d+\.\d+\.\d+$/.test(version)) {
    throw new Error(`version must be x.y.z, got ${version}`);
  }

  const template = JSON.parse(
    await readFile(path.join(root, 'npm', 'rom-cli', 'package.json'), 'utf8'),
  );
  const expected = platforms.map((platform) => `rom-cli-${platform.os}-${platform.cpu}`).sort();
  const declared = Object.keys(template.optionalDependencies).sort();
  if (expected.join() !== declared.join()) {
    throw new Error(
      `optionalDependencies must be ${expected.join(', ')}`,
    );
  }

  await mkdir(out, { recursive: true });
  const assets = path.join(out, 'assets');
  await mkdir(assets, { recursive: true });

  const sums = [];
  for (const platform of platforms) {
    const source = path.join(artifacts, platform.artifact, platform.binary);
    await access(source);
    const bytes = await readFile(source);
    const hash = createHash('sha256').update(bytes).digest('hex');
    sums.push(`${hash}  ${platform.asset}`);

    const pkgDir = path.join(out, `rom-cli-${platform.os}-${platform.cpu}`);
    await mkdir(pkgDir, { recursive: true });
    const binaryPath = path.join(pkgDir, platform.binary);
    await writeFile(binaryPath, bytes);
    if (platform.os !== 'win32') {
      await chmod(binaryPath, 0o755);
    }
    await writeFile(path.join(assets, platform.asset), bytes);
    if (platform.os !== 'win32') {
      await chmod(path.join(assets, platform.asset), 0o755);
    }
    await writeLicenses(pkgDir);
    await writeFile(
      path.join(pkgDir, 'package.json'),
      `${JSON.stringify(platformPackage(platform, version), null, 2)}\n`,
    );
  }

  sums.sort();
  await writeFile(path.join(assets, 'SHA256SUMS'), `${sums.join('\n')}\n`);

  const cliDir = path.join(out, 'rom-cli');
  await mkdir(path.join(cliDir, 'bin'), { recursive: true });
  await copyFile(
    path.join(root, 'npm', 'rom-cli', 'bin', 'rom.js'),
    path.join(cliDir, 'bin', 'rom.js'),
  );
  await chmod(path.join(cliDir, 'bin', 'rom.js'), 0o755);
  await copyFile(path.join(root, 'README.md'), path.join(cliDir, 'README.md'));
  await writeLicenses(cliDir);

  const optionalDependencies = {};
  for (const name of expected) {
    optionalDependencies[name] = version;
  }
  const cli = {
    ...template,
    version,
    optionalDependencies,
  };
  await writeFile(path.join(cliDir, 'package.json'), `${JSON.stringify(cli, null, 2)}\n`);
}

function platformPackage(platform, version) {
  const name = `rom-cli-${platform.os}-${platform.cpu}`;
  return {
    name,
    version,
    description: `Prebuilt rom binary for ${platform.os} ${platform.cpu}. Install rom-cli instead.`,
    license: 'MIT OR Apache-2.0',
    repository,
    os: [platform.os],
    cpu: [platform.cpu],
    files: [platform.binary, 'LICENSE-MIT', 'LICENSE-APACHE'],
    publishConfig: { access: 'public' },
  };
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
