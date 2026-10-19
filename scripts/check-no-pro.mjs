// Fails when code in this repository references closed Pro code.
// Varve Community must build without Pro, and Pro depends on Community,
// never the other way round (docs/open-core.md).

import { readdirSync, readFileSync, statSync } from 'node:fs';
import { join, relative } from 'node:path';

const root = new URL('..', import.meta.url).pathname;
const scanned = ['Cargo.toml', 'package.json', 'pnpm-workspace.yaml', 'crates', 'app', 'packages', 'scripts', '.github'];
const skipDirs = new Set(['node_modules', 'dist', 'target', 'gen', 'icons']);
const textFile = /\.(rs|toml|json|ya?ml|ts|vue|js|mjs|html|css)$/;
const self = 'scripts/check-no-pro.mjs';

// Names that only exist in the private varve-pro repository.
const forbidden = [
  /varve-pro/i,
  /\bpro-(diff|transfer|sync|stats|wrapped|cloud-client|teams)\b/,
  /@varve\/pro/,
  /\bvarve_pro\b/,
];

function* files(path) {
  const stat = statSync(path, { throwIfNoEntry: false });
  if (!stat) return;
  if (stat.isDirectory()) {
    for (const entry of readdirSync(path)) {
      if (!skipDirs.has(entry)) yield* files(join(path, entry));
    }
  } else if (textFile.test(path)) {
    yield path;
  }
}

const hits = [];
for (const entry of scanned) {
  for (const file of files(join(root, entry))) {
    const rel = relative(root, file);
    if (rel === self) continue;
    readFileSync(file, 'utf8')
      .split('\n')
      .forEach((line, index) => {
        if (forbidden.some((pattern) => pattern.test(line))) hits.push(`${rel}:${index + 1}: ${line.trim()}`);
      });
  }
}

if (hits.length > 0) {
  console.error('Community code must not reference Pro code:\n' + hits.join('\n'));
  process.exit(1);
}
console.log('OK: no references to Pro code.');
