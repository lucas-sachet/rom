'use strict';

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');
const test = require('node:test');

const repo = path.resolve(__dirname, '..', '..');
const packageDir = path.join(repo, 'npm', 'rom-cli');
const installed = path.join(packageDir, 'node_modules', 'rom-cli-win32-x64');
const { resolveBinary, PACKAGES } = require('../rom-cli/bin/rom.js');

test('wrapper lists every platform the release publishes', () => {
  assert.deepEqual(Object.keys(PACKAGES).sort(), [
    'linux arm64',
    'linux x64',
    'win32 x64',
  ]);
});

test('wrapper resolves and runs the binary installed beside rom-cli', (t) => {
  if (process.platform !== 'win32' || process.arch !== 'x64') {
    t.skip('this checkout exercise runs on Windows x64');
    return;
  }
  const built = path.join(repo, 'target', 'release', 'rom.exe');
  if (!fs.existsSync(built)) {
    t.skip('no local release binary');
    return;
  }

  fs.rmSync(path.join(packageDir, 'node_modules'), { recursive: true, force: true });
  fs.mkdirSync(installed, { recursive: true });
  fs.copyFileSync(built, path.join(installed, 'rom.exe'));
  fs.writeFileSync(
    path.join(installed, 'package.json'),
    JSON.stringify({ name: 'rom-cli-win32-x64', version: '0.0.0' }),
  );

  try {
    assert.equal(resolveBinary().exe, path.join(installed, 'rom.exe'));
    assert.ok(PACKAGES['linux x64']);
    const result = spawnSync(process.execPath, [path.join(packageDir, 'bin', 'rom.js'), '--version'], {
      encoding: 'utf8',
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /^rom 0\.1\.0/);
  } finally {
    fs.rmSync(path.join(packageDir, 'node_modules'), { recursive: true, force: true });
  }
});
