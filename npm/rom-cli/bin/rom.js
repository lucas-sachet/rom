#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const path = require('node:path');

const NATIVE_DIR = path.join(__dirname, 'native');

function resolveBinary({
  platform = process.platform,
  arch = process.arch,
  nativeDir = NATIVE_DIR,
} = {}) {
  const name = platform === 'win32' ? 'rom.exe' : 'rom';
  const exe = path.join(nativeDir, `${platform}-${arch}`, name);
  if (!fs.existsSync(exe)) {
    return {
      error: `rom: no prebuilt binary for ${platform} ${arch}. Supported: win32 x64, linux x64, linux arm64. See https://github.com/lucas-sachet/rom/releases`,
    };
  }
  return { exe };
}

// npm can drop the execute bit from files that are not `bin` entries. Restore it
// when the install owns the file. If that fails, spawn reports the error.
function ensureExecutable(exe) {
  if (process.platform === 'win32') {
    return;
  }
  try {
    fs.accessSync(exe, fs.constants.X_OK);
  } catch {
    try {
      fs.chmodSync(exe, 0o755);
    } catch {
      // Left for spawnSync to report.
    }
  }
}

function main() {
  const resolved = resolveBinary();
  if (resolved.error) {
    console.error(resolved.error);
    process.exit(1);
  }
  ensureExecutable(resolved.exe);

  const result = spawnSync(resolved.exe, process.argv.slice(2), {
    stdio: 'inherit',
    windowsHide: true,
  });
  if (result.error) {
    console.error(`rom: failed to start the binary: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status === null ? 1 : result.status);
}

if (require.main === module) {
  main();
} else {
  module.exports = { resolveBinary, ensureExecutable, NATIVE_DIR };
}
