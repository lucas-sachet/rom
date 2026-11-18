import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm, writeFile, mkdir } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import test from 'node:test';

import { pack, platforms } from '../prepare-publish.mjs';

test('pack builds one package per platform and the rom-cli wrapper', async () => {
  const dir = await mkdtemp(path.join(tmpdir(), 'rom-pack-'));
  try {
    const artifacts = path.join(dir, 'artifacts');
    for (const platform of platforms) {
      const folder = path.join(artifacts, platform.artifact);
      await mkdir(folder, { recursive: true });
      await writeFile(path.join(folder, platform.binary), `bin-${platform.artifact}`);
    }

    const out = path.join(dir, 'dist');
    await pack({ version: '0.1.0', artifacts, out });

    const cli = JSON.parse(await readFile(path.join(out, 'rom-cli', 'package.json'), 'utf8'));
    assert.equal(cli.name, 'rom-cli');
    assert.equal(cli.version, '0.1.0');
    assert.equal(cli.bin.rom, 'bin/rom.js');
    assert.equal(Object.keys(cli.optionalDependencies).length, platforms.length);
    for (const platform of platforms) {
      assert.equal(cli.optionalDependencies[`rom-cli-${platform.os}-${platform.cpu}`], '0.1.0');
      const packed = JSON.parse(
        await readFile(path.join(out, `rom-cli-${platform.os}-${platform.cpu}`, 'package.json'), 'utf8'),
      );
      assert.deepEqual(packed.os, [platform.os]);
      assert.deepEqual(packed.cpu, [platform.cpu]);
      const binary = await readFile(
        path.join(out, `rom-cli-${platform.os}-${platform.cpu}`, platform.binary),
        'utf8',
      );
      assert.equal(binary, `bin-${platform.artifact}`);
    }

    const sums = await readFile(path.join(out, 'assets', 'SHA256SUMS'), 'utf8');
    assert.equal(sums.trim().split('\n').length, platforms.length);
    assert.match(sums, /rom-win32-x64\.exe/);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
});
