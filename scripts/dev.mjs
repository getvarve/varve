// `pnpm dev`: runs `tauri dev`. On macOS, when the local "Varve Dev" code
// signing identity exists (scripts/macos/create-dev-signing-identity.sh),
// cargo runs the dev binary through scripts/macos/run-signed.sh, which
// signs it with that identity first. A dev binary is otherwise ad-hoc
// signed with a new identity on every rebuild, so the keychain's "Always
// Allow" never sticks and every build asks for the login password again
// (docs/macos-dev-signing.md). Other platforms only get the CLI started.

import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import process from 'node:process';
import { fileURLToPath, pathToFileURL } from 'node:url';

/**
 * The workspace's own Tauri CLI shim for a platform: `tauri.cmd` on
 * Windows (npm writes .cmd shims there), `tauri` elsewhere. Resolves to
 * `null` when the shim is not installed.
 */
export function resolveTauri(root, platform, exists = existsSync) {
  const shim = join(root, 'node_modules', '.bin', platform === 'win32' ? 'tauri.cmd' : 'tauri');
  return exists(shim) ? shim : null;
}

// cmd.exe metacharacters, escaped with ^ (see cross-spawn and
// http://www.robvanderwoude.com/escapechars.php).
const CMD_META = /([()\][%!^"`<>&|;, *?])/g;

/** Escapes the program path for cmd.exe: metacharacters only, no quotes. */
function escapeCmdCommand(value) {
  return String(value).replace(CMD_META, '^$1');
}

/**
 * Escapes one argument for a `.cmd` shim started through cmd.exe, the way
 * cross-spawn does: backslashes before a quote are doubled and the quote
 * escaped, trailing backslashes doubled, the whole thing quoted, then every
 * metacharacter escaped twice (cmd.exe parses the line once for /c and
 * once more for the shim).
 */
function escapeCmdArgument(value) {
  let arg = String(value);
  arg = arg.replace(/(?=(\\+?)?)\1"/g, '$1$1\\"');
  arg = arg.replace(/(?=(\\+?)?)\1$/, '$1$1');
  arg = `"${arg}"`;
  arg = arg.replace(CMD_META, '^$1');
  arg = arg.replace(CMD_META, '^$1');
  return arg;
}

/**
 * How to start the shim. A `.cmd` file cannot be started directly: it runs
 * through `cmd.exe /d /s /c "<one escaped command line>"` with verbatim
 * arguments, so no shell interpretation happens beyond the escaping above
 * (and nothing is passed as `shell: true`). Elsewhere the shim is started
 * directly with raw arguments.
 */
export function spawnPlan(shim, args) {
  if (shim.toLowerCase().endsWith('.cmd')) {
    const line = [escapeCmdCommand(shim), ...args.map(escapeCmdArgument)].join(' ');
    return { command: 'cmd.exe', args: ['/d', '/s', '/c', `"${line}"`], shell: false, windowsVerbatimArguments: true };
  }
  return { command: shim, args: [...args], shell: false, windowsVerbatimArguments: false };
}

/**
 * The environment for `tauri dev`: on macOS with the signing identity, the
 * cargo runner is set (unless the user set one) so the dev binary is
 * signed before it runs. Returns the environment and a message for the
 * user.
 */
export function signingEnv(env, platform, arch, identityExists, root) {
  const result = { ...env };
  if (platform !== 'darwin') return { env: result, message: null };
  const identity = env.VARVE_DEV_SIGNING_IDENTITY ?? 'Varve Dev';
  const triple = arch === 'arm64' ? 'AARCH64_APPLE_DARWIN' : 'X86_64_APPLE_DARWIN';
  const variable = `CARGO_TARGET_${triple}_RUNNER`;
  if (!identityExists) {
    return {
      env: result,
      message:
        `dev: no "${identity}" code signing identity; keychain prompts return after every rebuild.\n` +
        '     One-time setup: scripts/macos/create-dev-signing-identity.sh (see docs/macos-dev-signing.md)',
    };
  }
  if (result[variable]) {
    return {
      env: result,
      message: `dev: ${variable} is already set to ${result[variable]}; keeping it, so dev builds are only signed if that runner does it`,
    };
  }
  result[variable] = join(root, 'scripts', 'macos', 'run-signed.sh');
  return { env: result, message: `dev: signing dev builds with "${identity}" (${variable})` };
}

function identityExists(env) {
  const identity = env.VARVE_DEV_SIGNING_IDENTITY ?? 'Varve Dev';
  const found = spawnSync('security', ['find-identity', '-v', '-p', 'codesigning'], { encoding: 'utf8' });
  return found.status === 0 && found.stdout.includes(`"${identity}"`);
}

function main() {
  const root = dirname(dirname(fileURLToPath(import.meta.url)));
  const shim = resolveTauri(root, process.platform);
  if (!shim) {
    console.error(`dev: the Tauri CLI is missing under ${join(root, 'node_modules', '.bin')}; run pnpm install first`);
    process.exit(1);
  }
  const exists = process.platform === 'darwin' && identityExists(process.env);
  const { env, message } = signingEnv(process.env, process.platform, process.arch, exists, root);
  if (message) console.error(message);
  const plan = spawnPlan(shim, ['dev', ...process.argv.slice(2)]);
  const result = spawnSync(plan.command, plan.args, {
    stdio: 'inherit',
    env,
    cwd: root,
    shell: plan.shell,
    windowsVerbatimArguments: plan.windowsVerbatimArguments,
  });
  if (result.error) {
    console.error(`dev: could not start ${shim}: ${result.error.message}`);
    process.exit(1);
  }
  process.exit(result.status ?? 1);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main();
}
