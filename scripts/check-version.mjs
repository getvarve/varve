// Fails when a release tag does not match the versions in the repository.
// Usage: node scripts/check-version.mjs v1.2.3

import { readFileSync } from 'node:fs';

const tag = process.argv[2] ?? '';
const expected = tag.replace(/^v/, '');
if (!/^\d+\.\d+\.\d+$/.test(expected)) {
  console.error(`Tag "${tag}" is not of the form vMAJOR.MINOR.PATCH.`);
  process.exit(1);
}

const root = new URL('..', import.meta.url);
const read = (path) => readFileSync(new URL(path, root), 'utf8');
const cargo = /\[workspace\.package\][^[]*?\nversion = "([^"]+)"/s.exec(read('Cargo.toml'))?.[1];
const found = {
  'Cargo.toml [workspace.package]': cargo,
  'app/package.json': JSON.parse(read('app/package.json')).version,
  'packages/ui/package.json': JSON.parse(read('packages/ui/package.json')).version,
};

const wrong = Object.entries(found).filter(([, version]) => version !== expected);
if (wrong.length > 0) {
  console.error(`Tag ${tag} expects version ${expected}, but:\n` + wrong.map(([file, v]) => `  ${file}: ${v}`).join('\n'));
  process.exit(1);
}
console.log(`OK: every version is ${expected}.`);
