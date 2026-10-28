// Writes the "What's new" section of a release from the commit subjects since the previous tag.
// Commits follow Conventional Commits (`type(scope): summary`); users see features, fixes and
// performance work, without the type and scope. The update windows (Sparkle, the Tauri updater)
// show this section too.
//
// Usage: git log --format=%s <previous>..<tag> | node scripts/release-notes.mjs

import process from 'node:process';
import { pathToFileURL } from 'node:url';

const SHOWN = { feat: 'New', fix: 'Fixed', perf: 'Faster' };

/** Turns commit subjects into the Markdown section; null when nothing user-facing changed. */
export function whatsNew(subjects) {
  const groups = { feat: [], fix: [], perf: [] };
  for (const subject of subjects) {
    const match = /^(feat|fix|perf)(\([^)]*\))?!?: (.+)$/.exec(subject.trim());
    if (!match) continue;
    const summary = match[3].trim();
    groups[match[1]].push(summary.charAt(0).toUpperCase() + summary.slice(1));
  }
  const lines = [];
  for (const [type, label] of Object.entries(SHOWN)) {
    for (const summary of groups[type]) lines.push(`- **${label}:** ${summary}`);
  }
  return lines.length ? `## What's new\n\n${lines.join('\n')}\n` : null;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  let input = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (chunk) => (input += chunk));
  process.stdin.on('end', () => {
    const section = whatsNew(input.split('\n'));
    process.stdout.write(section ?? "## What's new\n\nMaintenance and small improvements.\n");
  });
}
