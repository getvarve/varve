// `node --test scripts/` (also `pnpm test:scripts`).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import {
  appcast,
  cdata,
  latestJson,
  parseSparkleSignature,
  TAURI_PLATFORMS,
  versionOf,
  xmlEscape,
} from './update-feeds.mjs';

const signatureOf = (asset) => `sig-of-${asset}`;

test('a release tag gives its version', () => {
  assert.equal(versionOf('v0.1.6'), '0.1.6');
  assert.throws(() => versionOf('0.1.6'), /vMAJOR/);
  assert.throws(() => versionOf('v0.1.6-rc.1'), /vMAJOR/);
});

test('latest.json lists every installer with its signature', () => {
  const feed = latestJson({ tag: 'v0.1.6', notes: 'Notes', pubDate: '2026-10-28T14:00:00Z', signatureOf });
  assert.equal(feed.version, '0.1.6');
  assert.equal(feed.pub_date, '2026-10-28T14:00:00Z');
  assert.deepEqual(Object.keys(feed.platforms).sort(), [
    'linux-aarch64',
    'linux-aarch64-appimage',
    'linux-aarch64-deb',
    'linux-x86_64',
    'linux-x86_64-appimage',
    'linux-x86_64-deb',
    'windows-x86_64',
    'windows-x86_64-msi',
    'windows-x86_64-nsis',
  ]);
  assert.equal(feed.platforms['linux-x86_64-deb'].url.endsWith('/varve-linux-x64.deb'), true);
  assert.deepEqual(feed.platforms['windows-x86_64'], {
    signature: 'sig-of-varve-windows-x64-setup.exe',
    url: 'https://github.com/getvarve/varve/releases/download/v0.1.6/varve-windows-x64-setup.exe',
  });
});

test('latest.json refuses a release with any installer unsigned', () => {
  // An unsigned deb would make deb installs fall back to the AppImage entry.
  const withoutDeb = (asset) => (asset.endsWith('.deb') ? null : `sig-of-${asset}`);
  assert.throws(
    () => latestJson({ tag: 'v0.1.6', notes: '', pubDate: '2026-10-28T14:00:00Z', signatureOf: withoutDeb }),
    /No updater signature for linux-x86_64-deb/,
  );
});

test('every updater platform points at a release asset name', () => {
  for (const [platform, asset] of TAURI_PLATFORMS) {
    assert.match(platform, /^(windows|linux)-(x86_64|aarch64)(-(nsis|msi|appimage|deb))?$/);
    assert.match(asset, /^varve-(windows|linux)-(x64|arm64)/);
  }
});

test('the sign_update line gives the signature and length', () => {
  assert.deepEqual(parseSparkleSignature('sparkle:edSignature="AbC+/12=" length="4242"\n'), {
    signature: 'AbC+/12=',
    length: '4242',
  });
  assert.throws(() => parseSparkleSignature('Error: no key'), /Not a sign_update line/);
});

test('release notes cannot break out of the appcast', () => {
  assert.equal(xmlEscape('a<b>&"c"'), 'a&lt;b&gt;&amp;&quot;c&quot;');
  assert.equal(cdata('x ]]> y'), '<![CDATA[x ]]]]><![CDATA[> y]]>');
  const xml = appcast({
    tag: 'v0.1.6',
    asset: 'varve-macos-arm64.app.tar.gz',
    notesHtml: '<p>Hi</p> ]]> <script>',
    pubDate: '2026-10-28T14:00:00Z',
    signature: 'AbC=',
    length: '10',
  });
  assert.match(xml, /<sparkle:version>0\.1\.6<\/sparkle:version>/);
  assert.match(xml, /<sparkle:fullReleaseNotesLink>https:\/\/github\.com\/getvarve\/varve\/releases\/tag\/v0\.1\.6<\/sparkle:fullReleaseNotesLink>/);
  assert.match(xml, /<pubDate>Wed, 28 Oct 2026 14:00:00 GMT<\/pubDate>/);
  assert.match(
    xml,
    /<enclosure url="https:\/\/github\.com\/getvarve\/varve\/releases\/download\/v0\.1\.6\/varve-macos-arm64\.app\.tar\.gz" sparkle:edSignature="AbC=" length="10" type="application\/octet-stream"\/>/,
  );
  // The only CDATA end markers are the section boundaries.
  assert.equal(xml.split(']]>').length - 1, 2);
});
