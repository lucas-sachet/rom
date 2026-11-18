'use strict';

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');

const repo = path.resolve(__dirname, '..', '..');
const { resolveBinary, ensureExecutable, NATIVE_DIR } = require('../rom-cli/bin/rom.js');

test('wrapper finds the binary bundled for the platform', () => {
  const nativeDir = fs.mkdtempSync(path.join(os.tmpdir(), 'rom-native-'));
  try {
    const folder = path.join(nativeDir, 'linux-x64');
    fs.mkdirSync(folder);
    fs.writeFileSync(path.join(folder, 'rom'), '');

    assert.equal(
      resolveBinary({ platform: 'linux', arch: 'x64', nativeDir }).exe,
      path.join(folder, 'rom'),
    );
    assert.match(resolveBinary({ platform: 'win32', arch: 'x64', nativeDir }).error, /no prebuilt binary/);
    assert.match(resolveBinary({ platform: 'darwin', arch: 'arm64', nativeDir }).error, /no prebuilt binary/);
  } finally {
    fs.rmSync(nativeDir, { recursive: true, force: true });
  }
});

test('wrapper restores the execute bit on Unix', { skip: process.platform === 'win32' }, () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'rom-exec-'));
  try {
    const file = path.join(dir, 'rom');
    fs.writeFileSync(file, '');
    fs.chmodSync(file, 0o644);
    ensureExecutable(file);
    fs.accessSync(file, fs.constants.X_OK);
  } finally {
    fs.rmSync(dir, { recursive: true, force: true });
  }
});

test('wrapper runs the bundled binary', (t) => {
  if (process.platform !== 'win32' || process.arch !== 'x64') {
    t.skip('this checkout exercise runs on Windows x64');
    return;
  }
  const built = path.join(repo, 'target', 'release', 'rom.exe');
  if (!fs.existsSync(built)) {
    t.skip('no local release binary');
    return;
  }

  const folder = path.join(NATIVE_DIR, 'win32-x64');
  const bundled = path.join(folder, 'rom.exe');
  fs.mkdirSync(folder, { recursive: true });
  fs.copyFileSync(built, bundled);
  try {
    const result = spawnSync(process.execPath, [path.join(__dirname, '..', 'rom-cli', 'bin', 'rom.js'), '--version'], {
      encoding: 'utf8',
    });
    assert.equal(result.status, 0, result.stderr);
    assert.match(result.stdout, /^rom \d+\.\d+\.\d+/);
  } finally {
    fs.rmSync(folder, { recursive: true, force: true });
  }
});
