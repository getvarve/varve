// Fetches the pinned Sparkle release for macOS builds into app/src-tauri/macos/sparkle/:
// Sparkle.framework (bundled through bundle.macOS.frameworks in tauri.conf.json) and its bin/
// tools (sign_update signs the release in CI). Runs before every build (beforeBuildCommand);
// it does nothing off macOS or when the pinned version is already in place. The download is
// checked against the pinned SHA-256 before anything is unpacked.

import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import process from 'node:process';
import { fileURLToPath, pathToFileURL } from 'node:url';

export const SPARKLE_VERSION = '2.10.0';
export const SPARKLE_SHA256 = 'c2bf58aa8387266ac179357b1415d6f2635f044da8be41042af32425dae6da0c';
const URL_BASE = 'https://github.com/sparkle-project/Sparkle/releases/download';

/** Throws unless `bytes` hash to `expected` (hex SHA-256). */
export function verifySha256(bytes, expected) {
  const actual = createHash('sha256').update(bytes).digest('hex');
  if (actual !== expected) {
    throw new Error(`Sparkle download checksum mismatch: expected ${expected}, got ${actual}`);
  }
}

/** Where the unpacked release lives, and the marker that records its version. */
export function sparkleDir(root) {
  return join(root, 'app', 'src-tauri', 'macos', 'sparkle');
}

/** True when `dir` already holds `version` with its framework. */
export function isInstalled(dir, version, exists = existsSync, read = readFileSync) {
  const marker = join(dir, 'VERSION');
  return (
    exists(join(dir, 'Sparkle.framework')) &&
    exists(marker) &&
    read(marker, 'utf8').trim() === version
  );
}

async function fetchSparkle(root) {
  const dir = sparkleDir(root);
  if (isInstalled(dir, SPARKLE_VERSION)) return;
  const url = `${URL_BASE}/${SPARKLE_VERSION}/Sparkle-${SPARKLE_VERSION}.tar.xz`;
  const response = await fetch(url);
  if (!response.ok) throw new Error(`Could not download ${url}: HTTP ${response.status}`);
  const bytes = Buffer.from(await response.arrayBuffer());
  verifySha256(bytes, SPARKLE_SHA256);

  // Unpack next to the target and swap it in, so a failed run never leaves half a framework
  // (and the renames stay on one volume).
  mkdirSync(dirname(dir), { recursive: true });
  const work = mkdtempSync(join(dirname(dir), '.sparkle-'));
  const archive = join(work, 'Sparkle.tar.xz');
  writeFileSync(archive, bytes);
  const unpacked = join(work, 'sparkle');
  mkdirSync(unpacked);
  const tar = spawnSync('tar', ['-xJf', archive, '-C', unpacked], { stdio: 'inherit' });
  if (tar.status !== 0) throw new Error('Could not unpack the Sparkle release');
  writeFileSync(join(unpacked, 'VERSION'), `${SPARKLE_VERSION}\n`);
  if (existsSync(dir)) renameSync(dir, join(work, 'previous'));
  renameSync(unpacked, dir);
  console.log(`Sparkle ${SPARKLE_VERSION} is in ${dir}`);
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  if (process.platform === 'darwin') {
    const root = join(dirname(fileURLToPath(import.meta.url)), '..');
    fetchSparkle(root).catch((error) => {
      console.error(error.message);
      process.exit(1);
    });
  }
}
