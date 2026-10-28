// `node --test scripts/` (also `pnpm test:scripts`).
import assert from 'node:assert/strict';
import { Buffer } from 'node:buffer';
import { test } from 'node:test';
import {
  appcast,
  buildOf,
  cdata,
  latestJson,
  parseSparkleSignature,
  signedVersionOf,
  TAURI_PLATFORMS,
  versionOf,
  xmlEscape,
} from './update-feeds.mjs';

/** A `<asset>.sig` as `tauri signer sign --app-version` writes it: the minisign file in base64. */
function sig(asset, trusted) {
  const text = [
    'untrusted comment: signature from tauri secret key',
    'RUTX5qZvdPSepPF2zsjvP/L/TQtBLRnWRvaq+4omK1UEugtQp/USrGiI1v762sDy3id3aor7bRC9ETyeot6lMHHrWoZ5hjUM1Qc=',
    `trusted comment: ${trusted}`,
    'DDq2CLOEl5cQmC2eJRHlnZPVViOZsu1CjpPKQoei4A/XBfkDqGO0B+QMrcrHgEQ5qV3mne75tdFWhJpqUXh9AA==',
  ].join('\n');
  return Buffer.from(`${text}\n`).toString('base64');
}

const signatureOf = (asset) => sig(asset, `timestamp:1791649051\tfile:${asset}\tversion:0.1.6`);

test('a release tag gives its version', () => {
  assert.equal(versionOf('v0.1.6'), '0.1.6');
  assert.throws(() => versionOf('0.1.6'), /vMAJOR/);
  assert.throws(() => versionOf('v0.1.6-rc.1'), /vMAJOR/);
});

test('latest.json lists every installer with its signature', () => {
  const feed = latestJson({ tag: 'v0.1.6', notes: 'Notes', pubDate: '2026-10-28T14:00:00Z', signatureOf });
  assert.equal(feed.version, '0.1.6');
  assert.equal(feed.pub_date, '2026-10-28T14:00:00Z');
  // The deb and msi entries serve only installs from 0.1.7 and older (see TAURI_PLATFORMS).
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
  assert.equal(feed.platforms['linux-x86_64'].url.endsWith('/varve-linux-x64.AppImage'), true);
  assert.equal(feed.platforms['linux-x86_64-deb'].url.endsWith('/varve-linux-x64.deb'), true);
  assert.equal(feed.platforms['windows-x86_64-msi'].url.endsWith('/varve-windows-x64.msi'), true);
  assert.deepEqual(feed.platforms['windows-x86_64'], {
    signature: signatureOf('varve-windows-x64-setup.exe'),
    url: 'https://github.com/getvarve/varve/releases/download/v0.1.6/varve-windows-x64-setup.exe',
  });
});

test('latest.json refuses a release with any installer unsigned', () => {
  // An unsigned deb would make a 0.1.7 deb install fall back to the AppImage entry.
  const withoutDeb = (asset) => (asset.endsWith('.deb') ? null : signatureOf(asset));
  assert.throws(
    () => latestJson({ tag: 'v0.1.6', notes: '', pubDate: '2026-10-28T14:00:00Z', signatureOf: withoutDeb }),
    /No updater signature for linux-x86_64-deb/,
  );
  const withoutAppImage = (asset) => (asset.endsWith('.AppImage') ? null : signatureOf(asset));
  assert.throws(
    () => latestJson({ tag: 'v0.1.6', notes: '', pubDate: '2026-10-28T14:00:00Z', signatureOf: withoutAppImage }),
    /No updater signature for linux-x86_64$/,
  );
});

test('latest.json refuses a signature not bound to the release version', () => {
  const release = { tag: 'v0.1.6', notes: '', pubDate: '2026-10-28T14:00:00Z' };
  // Signed without --app-version: the app (requireSignedVersion) would reject it.
  const unversioned = (asset) => sig(asset, `timestamp:1791649051\tfile:${asset}`);
  assert.throws(() => latestJson({ ...release, signatureOf: unversioned }), /is for version \(none\), not 0\.1\.6/);
  // An older release's signature replayed under the new version.
  const older = (asset) => sig(asset, `timestamp:1791649051\tfile:${asset}\tversion:0.1.5`);
  assert.throws(() => latestJson({ ...release, signatureOf: older }), /is for version 0\.1\.5, not 0\.1\.6/);
});

test('the signed version is read from the trusted comment', () => {
  assert.equal(signedVersionOf(sig('a', 'timestamp:1\tfile:a\tversion:1.2.3')), '1.2.3');
  assert.equal(signedVersionOf(sig('a', 'timestamp:1\tfile:a')), null);
  assert.equal(signedVersionOf(Buffer.from('not a signature').toString('base64')), null);
});

test('every updater platform points at the matching kind of installer', () => {
  const kinds = { nsis: /-setup\.exe$/, msi: /\.msi$/, appimage: /\.AppImage$/, deb: /\.deb$/ };
  for (const [platform, asset] of TAURI_PLATFORMS) {
    const match = /^(windows|linux)-(x86_64|aarch64)(?:-(nsis|msi|appimage|deb))?$/.exec(platform);
    assert.ok(match, platform);
    // The plain entry is what new builds read: the NSIS setup or the AppImage.
    const kind = match[3] ?? (match[1] === 'windows' ? 'nsis' : 'appimage');
    assert.match(asset, kinds[kind], platform);
    assert.match(asset, /^varve-(windows|linux)-(x64|arm64)/);
  }
});

test('a build number is a positive integer', () => {
  assert.equal(buildOf(412), '412');
  assert.equal(buildOf('1'), '1');
  for (const bad of ['0', '-3', '1.2', 'v12', '', '012']) {
    assert.throws(() => buildOf(bad), /not a positive integer/);
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
    build: '412',
    asset: 'varve-macos-arm64.app.tar.gz',
    notesHtml: '<p>Hi</p> ]]> <script>',
    pubDate: '2026-10-28T14:00:00Z',
    signature: 'AbC=',
    length: '10',
  });
  // Sparkle orders by the build number; people see the version.
  assert.match(xml, /<sparkle:version>412<\/sparkle:version>/);
  assert.match(xml, /<sparkle:shortVersionString>0\.1\.6<\/sparkle:shortVersionString>/);
  assert.match(xml, /<sparkle:fullReleaseNotesLink>https:\/\/github\.com\/getvarve\/varve\/releases\/tag\/v0\.1\.6<\/sparkle:fullReleaseNotesLink>/);
  assert.match(xml, /<pubDate>Wed, 28 Oct 2026 14:00:00 GMT<\/pubDate>/);
  assert.match(
    xml,
    /<enclosure url="https:\/\/github\.com\/getvarve\/varve\/releases\/download\/v0\.1\.6\/varve-macos-arm64\.app\.tar\.gz" sparkle:edSignature="AbC=" length="10" type="application\/octet-stream"\/>/,
  );
  // The only CDATA end markers are the section boundaries.
  assert.equal(xml.split(']]>').length - 1, 2);
});
