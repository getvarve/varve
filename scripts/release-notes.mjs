// Writes the "What's new" section of a release from the commit subjects since the previous tag.
// Commits follow Conventional Commits (`type(scope): summary`); users see features, fixes and
// performance work, without the type and scope. The update windows (Sparkle, the Tauri updater)
// show this section too. Commits with the scope `server` get their own section, which the update windows do not show.
//
// Usage: git log --format=%s <previous>..<tag> | node scripts/release-notes.mjs

import process from 'node:process';
import { pathToFileURL } from 'node:url';

const SHOWN = { feat: 'New', fix: 'Fixed', perf: 'Faster' };

/** The scope of commits to the Varve server. They are not changes to the app a user updates. */
const SERVER_SCOPE = 'server';

/** The lines of one section: the user-facing commits, of the server or of everything else. */
function lines(subjects, server) {
  const groups = { feat: [], fix: [], perf: [] };
  for (const subject of subjects) {
    const match = /^(feat|fix|perf)(?:\(([^)]*)\))?!?: (.+)$/.exec(subject.trim());
    if (!match || (match[2] === SERVER_SCOPE) !== server) continue;
    const summary = match[3].trim();
    groups[match[1]].push(summary.charAt(0).toUpperCase() + summary.slice(1));
  }
  const out = [];
  for (const [type, label] of Object.entries(SHOWN)) {
    for (const summary of groups[type]) out.push(`- **${label}:** ${summary}`);
  }
  return out;
}

/**
 * Turns commit subjects into the app's Markdown section; null when nothing user-facing changed in the app.
 * Commits to the server are left out: the update windows show this section to people updating the app.
 */
export function whatsNew(subjects) {
  const out = lines(subjects, false);
  return out.length ? `## What's new\n\n${out.join('\n')}\n` : null;
}

/**
 * The server's section of the release page; null when the server did not change. The release workflow leaves
 * this section out of the update windows, like the install section.
 */
export function serverNotes(subjects) {
  const out = lines(subjects, true);
  return out.length ? `## Varve server\n\n${out.join('\n')}\n` : null;
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  let input = '';
  process.stdin.setEncoding('utf8');
  process.stdin.on('data', (chunk) => (input += chunk));
  process.stdin.on('end', () => {
    const subjects = input.split('\n');
    const server = serverNotes(subjects);
    process.stdout.write(whatsNew(subjects) ?? "## What's new\n\nMaintenance and small improvements.\n");
    if (server) process.stdout.write(`\n${server}`);
  });
}
