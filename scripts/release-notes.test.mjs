// `node --test scripts/` (also `pnpm test:scripts`).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { whatsNew } from './release-notes.mjs';

test('features, fixes and speed-ups make the section, in that order', () => {
  const notes = whatsNew([
    'fix(app): keep installer window labels readable in light and dark mode',
    'chore(repo): release 0.1.6',
    'feat(ui): turn the interface into glass',
    'perf(catalog): load large schemas faster',
    'ci(ci): pin runners',
    'feat!: drop an old setting',
  ]);
  assert.equal(
    notes,
    "## What's new\n\n" +
      '- **New:** Turn the interface into glass\n' +
      '- **New:** Drop an old setting\n' +
      '- **Fixed:** Keep installer window labels readable in light and dark mode\n' +
      '- **Faster:** Load large schemas faster\n',
  );
});

test('a release without user-facing commits has no section', () => {
  assert.equal(whatsNew(['chore(repo): release 0.1.6', 'docs(docs): typo', '']), null);
});
