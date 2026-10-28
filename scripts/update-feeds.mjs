// Writes the update feeds of a release from the signed assets in a folder:
//   latest.json          the Tauri updater feed (Windows, Linux)
//   appcast-arm64.xml    the Sparkle feed for Apple silicon
//   appcast-x64.xml      the Sparkle feed for Intel Macs
// The release workflow runs it after the bundle jobs; the apps read the feeds from
// https://github.com/getvarve/varve/releases/latest/download/<feed>.
//
// Usage: node scripts/update-feeds.mjs <dist> <tag> <notes.md> <notes.html>
//   <dist> holds the release assets, their Tauri updater signatures (<asset>.sig) and the Sparkle
//   signatures (<asset>.sparkle, the attribute line sign_update prints).

import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import process from 'node:process';
import { pathToFileURL } from 'node:url';

const REPO = 'getvarve/varve';

/** Tauri updater platforms and the release asset each installs from. */
export const TAURI_PLATFORMS = [
  ['windows-x86_64', 'varve-windows-x64-setup.exe'],
  ['windows-x86_64-nsis', 'varve-windows-x64-setup.exe'],
  ['windows-x86_64-msi', 'varve-windows-x64.msi'],
  ['linux-x86_64', 'varve-linux-x64.AppImage'],
  ['linux-x86_64-appimage', 'varve-linux-x64.AppImage'],
  ['linux-x86_64-deb', 'varve-linux-x64.deb'],
  ['linux-aarch64', 'varve-linux-arm64.AppImage'],
  ['linux-aarch64-appimage', 'varve-linux-arm64.AppImage'],
  ['linux-aarch64-deb', 'varve-linux-arm64.deb'],
];

/** Sparkle feeds and the archive each one offers. */
export const SPARKLE_FEEDS = [
  ['appcast-arm64.xml', 'varve-macos-arm64.app.tar.gz'],
  ['appcast-x64.xml', 'varve-macos-x64.app.tar.gz'],
];

/** The oldest macOS the app runs on (Tauri's default bundle.macOS.minimumSystemVersion). */
export const MINIMUM_MACOS = '10.13';

/** `v1.2.3` to `1.2.3`; throws for anything else. */
export function versionOf(tag) {
  const match = /^v(\d+\.\d+\.\d+)$/.exec(tag);
  if (!match) throw new Error(`Tag "${tag}" is not of the form vMAJOR.MINOR.PATCH`);
  return match[1];
}

function assetUrl(tag, asset) {
  return `https://github.com/${REPO}/releases/download/${tag}/${asset}`;
}

/**
 * The Tauri updater feed. Every platform must have a signature: the updater falls back from a
 * missing installer-specific entry (an MSI or deb install) to the plain one, which would install
 * the wrong kind of package.
 */
export function latestJson({ tag, notes, pubDate, signatureOf }) {
  const platforms = {};
  for (const [platform, asset] of TAURI_PLATFORMS) {
    const signature = signatureOf(asset);
    if (signature) platforms[platform] = { signature, url: assetUrl(tag, asset) };
  }
  for (const [platform] of TAURI_PLATFORMS) {
    if (!platforms[platform]) throw new Error(`No updater signature for ${platform}`);
  }
  return { version: versionOf(tag), notes, pub_date: pubDate, platforms };
}

/** Escapes text for an XML attribute or element. */
export function xmlEscape(text) {
  return text.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
}

/** Wraps HTML in CDATA; a literal `]]>` inside is split across two sections. */
export function cdata(html) {
  return `<![CDATA[${html.replaceAll(']]>', ']]]]><![CDATA[>')}]]>`;
}

/** Reads `sparkle:edSignature="…" length="…"` as printed by sign_update. */
export function parseSparkleSignature(line) {
  const signature = /sparkle:edSignature="([A-Za-z0-9+/=]+)"/.exec(line)?.[1];
  const length = /length="(\d+)"/.exec(line)?.[1];
  if (!signature || !length) throw new Error(`Not a sign_update line: ${line}`);
  return { signature, length };
}

/** A Sparkle appcast with the one release. */
export function appcast({ tag, asset, notesHtml, pubDate, signature, length }) {
  const version = versionOf(tag);
  return `<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>Varve</title>
    <link>https://github.com/${REPO}</link>
    <item>
      <title>Varve ${xmlEscape(version)}</title>
      <pubDate>${xmlEscape(new Date(pubDate).toUTCString())}</pubDate>
      <sparkle:version>${xmlEscape(version)}</sparkle:version>
      <sparkle:shortVersionString>${xmlEscape(version)}</sparkle:shortVersionString>
      <sparkle:minimumSystemVersion>${MINIMUM_MACOS}</sparkle:minimumSystemVersion>
      <sparkle:fullReleaseNotesLink>https://github.com/${REPO}/releases/tag/${xmlEscape(tag)}</sparkle:fullReleaseNotesLink>
      <description>${cdata(notesHtml)}</description>
      <enclosure url="${xmlEscape(assetUrl(tag, asset))}" sparkle:edSignature="${xmlEscape(signature)}" length="${xmlEscape(length)}" type="application/octet-stream"/>
    </item>
  </channel>
</rss>
`;
}

function main([dist, tag, notesFile, notesHtmlFile]) {
  if (!dist || !tag || !notesFile || !notesHtmlFile) {
    throw new Error('Usage: node scripts/update-feeds.mjs <dist> <tag> <notes.md> <notes.html>');
  }
  const notes = readFileSync(notesFile, 'utf8').trim();
  const notesHtml = readFileSync(notesHtmlFile, 'utf8').trim();
  const pubDate = new Date().toISOString().replace(/\.\d{3}Z$/, 'Z');
  const signatureOf = (asset) => {
    const path = join(dist, `${asset}.sig`);
    return existsSync(path) ? readFileSync(path, 'utf8').trim() : null;
  };
  writeFileSync(join(dist, 'latest.json'), `${JSON.stringify(latestJson({ tag, notes, pubDate, signatureOf }), null, 2)}\n`);
  for (const [feed, asset] of SPARKLE_FEEDS) {
    const { signature, length } = parseSparkleSignature(readFileSync(join(dist, `${asset}.sparkle`), 'utf8'));
    writeFileSync(join(dist, feed), appcast({ tag, asset, notesHtml, pubDate, signature, length }));
  }
  console.log(`Wrote latest.json and ${SPARKLE_FEEDS.map(([feed]) => feed).join(', ')} for ${tag}`);
}

if (import.meta.url === pathToFileURL(process.argv[1] ?? '').href) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(error.message);
    process.exit(1);
  }
}
