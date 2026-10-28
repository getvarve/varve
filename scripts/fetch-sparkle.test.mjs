// `node --test scripts/` (also `pnpm test:scripts`).
import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { join } from 'node:path';
import { test } from 'node:test';
import { isInstalled, sparkleDir, verifySha256 } from './fetch-sparkle.mjs';

test('a download with another checksum is refused', () => {
  const bytes = Buffer.from('not sparkle');
  const good = createHash('sha256').update(bytes).digest('hex');
  assert.doesNotThrow(() => verifySha256(bytes, good));
  assert.throws(() => verifySha256(bytes, '0'.repeat(64)), /checksum mismatch/);
});

test('the pinned version counts as installed only with its framework and marker', () => {
  const dir = sparkleDir(join('/', 'ws'));
  assert.equal(dir, join('/', 'ws', 'app', 'src-tauri', 'macos', 'sparkle'));
  const present = new Set([join(dir, 'Sparkle.framework'), join(dir, 'VERSION')]);
  const exists = (path) => present.has(path);
  assert.equal(isInstalled(dir, '2.10.0', exists, () => '2.10.0\n'), true);
  assert.equal(isInstalled(dir, '2.10.0', exists, () => '2.9.6\n'), false);
  assert.equal(isInstalled(dir, '2.10.0', (path) => path.endsWith('VERSION'), () => '2.10.0'), false);
});
