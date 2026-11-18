import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile, mkdir, stat } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { pack, platforms } from '../prepare-publish.mjs';

test('pack bundles every binary inside the rom-cli package', async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'rom-pack-'));
  try {
    const artifacts = path.join(dir, 'artifacts');
    for (const platform of platforms) {
      const folder = path.join(artifacts, platform.artifact);
      await mkdir(folder, { recursive: true });
      await writeFile(path.join(folder, platform.binary), `bin-${platform.artifact}`);
    }

    const out = path.join(dir, 'dist');
    await pack({ version: '0.1.1', artifacts, out });

    const cliDir = path.join(out, 'rom-cli');
    const cli = JSON.parse(await readFile(path.join(cliDir, 'package.json'), 'utf8'));
    assert.equal(cli.name, 'rom-cli');
    assert.equal(cli.version, '0.1.1');
    assert.equal(cli.bin.rom, 'bin/rom.js');
    assert.equal(cli.scripts, undefined, 'install must not run any script');
    assert.equal(cli.optionalDependencies, undefined);
    assert.deepEqual(cli.files.filter((file) => file.startsWith('bin/')), [
      'bin/rom.js',
      'bin/native/',
    ]);

    for (const platform of platforms) {
      const bundled = path.join(cliDir, 'bin', 'native', `${platform.os}-${platform.cpu}`, platform.binary);
      assert.equal(await readFile(bundled, 'utf8'), `bin-${platform.artifact}`);
    }
    await assert.rejects(stat(path.join(cliDir, 'bin', 'install.js')));

    const sums = await readFile(path.join(out, 'assets', 'SHA256SUMS'), 'utf8');
    assert.equal(sums.trim().split('\n').length, platforms.length);
    assert.match(sums, /rom-win32-x64\.exe/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
