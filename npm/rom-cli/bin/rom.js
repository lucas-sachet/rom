#!/usr/bin/env node
'use strict';

const { spawnSync } = require('node:child_process');
const path = require('node:path');

// Keys must match the platforms in npm/prepare-publish.mjs.
const PACKAGES = {
  'win32 x64': ['rom-cli-win32-x64', 'rom.exe'],
  'linux x64': ['rom-cli-linux-x64', 'rom'],
  'linux arm64': ['rom-cli-linux-arm64', 'rom'],
};

function resolveBinary() {
  const key = `${process.platform} ${process.arch}`;
  const entry = PACKAGES[key];
  if (!entry) {
    return {
      error: `rom: no prebuilt binary for ${process.platform} ${process.arch}. See https://github.com/lucas-sachet/rom/releases`,
    };
  }

  const [pkg, file] = entry;
  let packageJson;
  try {
    packageJson = require.resolve(`${pkg}/package.json`);
  } catch {
    return {
      error: `rom: ${pkg} is not installed. Reinstall with \`npm install -g rom-cli\`, or download a binary from https://github.com/lucas-sachet/rom/releases`,
    };
  }

  return { exe: path.join(path.dirname(packageJson), file) };
}

function main() {
  const resolved = resolveBinary();
  if (resolved.error) {
    console.error(resolved.error);
    process.exit(1);
  }

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
  module.exports = { resolveBinary, PACKAGES };
}
