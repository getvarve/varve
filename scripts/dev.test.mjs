// `node --test scripts/` (also `pnpm test:scripts`).
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { join } from 'node:path';
import { resolveTauri, signingEnv, spawnPlan } from './dev.mjs';

const root = join('/', 'ws');

test('the Tauri shim is tauri.cmd on Windows and tauri elsewhere', () => {
  const seen = [];
  const exists = (path) => {
    seen.push(path);
    return true;
  };
  assert.equal(resolveTauri(root, 'win32', exists), join(root, 'node_modules', '.bin', 'tauri.cmd'));
  assert.equal(resolveTauri(root, 'darwin', exists), join(root, 'node_modules', '.bin', 'tauri'));
  assert.equal(resolveTauri(root, 'linux', exists), join(root, 'node_modules', '.bin', 'tauri'));
  assert.equal(seen.length, 3);
  assert.equal(resolveTauri(root, 'win32', () => false), null);
});

const cmdShim = 'C:\\ws dir\\node_modules\\.bin\\tauri.cmd';

/** The command line a `.cmd` plan hands to cmd.exe. */
function cmdLine(args) {
  const plan = spawnPlan(cmdShim, args);
  assert.equal(plan.command, 'cmd.exe');
  assert.equal(plan.shell, false, 'never shell: true (DEP0190)');
  assert.equal(plan.windowsVerbatimArguments, true);
  assert.deepEqual(plan.args.slice(0, 3), ['/d', '/s', '/c']);
  assert.equal(plan.args.length, 4);
  return plan.args[3];
}

test('a .cmd shim runs through cmd.exe /d /s /c with one escaped command line', () => {
  // The program path gets its space escaped, never quotes; the line is quoted as a whole.
  assert.equal(cmdLine(['dev']), '"C:\\ws^ dir\\node_modules\\.bin\\tauri.cmd ^^^"dev^^^""');
});

test('cmd.exe metacharacters in arguments are escaped the way cross-spawn does', () => {
  // Expected values come from cross-spawn 7.0.6 escape.argument(value, true).
  const cases = [
    ['a&b', '^^^"a^^^&b^^^"'],
    ['x|y', '^^^"x^^^|y^^^"'],
    ['100%', '^^^"100^^^%^^^"'],
    ['up^', '^^^"up^^^^^^^"'],
    ['(p) !q', '^^^"^^^(p^^^)^^^ ^^^!q^^^"'],
    ['a<b>c', '^^^"a^^^<b^^^>c^^^"'],
  ];
  for (const [value, expected] of cases) {
    assert.equal(cmdLine([value]), `"C:\\ws^ dir\\node_modules\\.bin\\tauri.cmd ${expected}"`, value);
  }
});

test('inner quotes and backslashes are escaped, also after a quote', () => {
  const cases = [
    ['say "hi"', '^^^"say^^^ \\^^^"hi\\^^^"^^^"'],
    ['{"a": "b c"}', '^^^"{\\^^^"a\\^^^":^^^ \\^^^"b^^^ c\\^^^"}^^^"'],
    ['q"&after', '^^^"q\\^^^"^^^&after^^^"'],
    ['back\\"slash', '^^^"back\\\\\\^^^"slash^^^"'],
    ['trail\\', '^^^"trail\\\\^^^"'],
  ];
  for (const [value, expected] of cases) {
    assert.equal(cmdLine([value]), `"C:\\ws^ dir\\node_modules\\.bin\\tauri.cmd ${expected}"`, value);
  }
  // Several arguments stay separate tokens.
  assert.equal(
    cmdLine(['dev', '--config', '{"a":1}']),
    '"C:\\ws^ dir\\node_modules\\.bin\\tauri.cmd ^^^"dev^^^" ^^^"--config^^^" ^^^"{\\^^^"a\\^^^":1}^^^""',
  );
});

test('a plain shim is started directly with raw arguments', () => {
  const plan = spawnPlan('/ws/node_modules/.bin/tauri', ['dev', '--config', '{"a": "b c"}']);
  assert.equal(plan.shell, false);
  assert.equal(plan.windowsVerbatimArguments, false);
  assert.equal(plan.command, '/ws/node_modules/.bin/tauri');
  assert.deepEqual(plan.args, ['dev', '--config', '{"a": "b c"}']);
});

test('signing only happens on macOS with the identity', () => {
  const base = { PATH: '/bin' };
  const runner = join(root, 'scripts', 'macos', 'run-signed.sh');
  for (const platform of ['win32', 'linux']) {
    const { env, message } = signingEnv(base, platform, 'x64', true, root);
    assert.deepEqual(env, base, platform);
    assert.equal(message, null, platform);
  }
  const arm = signingEnv(base, 'darwin', 'arm64', true, root);
  assert.equal(arm.env.CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER, runner);
  assert.match(arm.message, /signing dev builds with "Varve Dev"/);
  const intel = signingEnv(base, 'darwin', 'x64', true, root);
  assert.equal(intel.env.CARGO_TARGET_X86_64_APPLE_DARWIN_RUNNER, runner);
  const missing = signingEnv(base, 'darwin', 'arm64', false, root);
  assert.deepEqual(missing.env, base);
  assert.match(missing.message, /no "Varve Dev" code signing identity/);
});

test('a runner the user set is kept and said so', () => {
  const base = { CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER: '/my/runner' };
  const { env, message } = signingEnv(base, 'darwin', 'arm64', true, root);
  assert.equal(env.CARGO_TARGET_AARCH64_APPLE_DARWIN_RUNNER, '/my/runner');
  assert.match(message, /already set to \/my\/runner; keeping it/);
  assert.doesNotMatch(message, /signing dev builds/);
});

test('a custom identity name is honoured', () => {
  const { message } = signingEnv({ VARVE_DEV_SIGNING_IDENTITY: 'Mine' }, 'darwin', 'arm64', false, root);
  assert.match(message, /no "Mine" code signing identity/);
});
