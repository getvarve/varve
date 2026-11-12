/**
 * Shared text for tooltips (a `title` in a template, see lib/tooltips.ts): the reason a control is off, a button's
 * keyboard shortcut written the way the system writes it, and what short marks (a tab's coloured edge, a Live pill,
 * a key's type tag) stand for.
 */

import { MODULE_KIND_LABELS, moduleKindOf } from '../api/redis';
import type { KeyType } from '../api/redis';

/** Why a change is off on a read-only connection. Every control that a read-only session disables says this. */
export const READ_ONLY_TIP = 'The connection is read only';

/** What a "Read only" badge or lock means. */
export const READ_ONLY_BADGE_TIP = 'The connection is set to read only, so nothing can change data';

/** Whether the app runs on macOS (createVarveApp.ts sets `data-platform` on the document). */
export function isMacPlatform(): boolean {
  return globalThis.document?.documentElement.dataset.platform === 'mac';
}

/** macOS writes modifiers as symbols in this order, with no plus signs. */
const MAC_ORDER = ['Ctrl', 'Alt', 'Shift', 'Cmd'] as const;
const MAC_SYMBOL: Record<(typeof MAC_ORDER)[number], string> = { Ctrl: '⌃', Alt: '⌥', Shift: '⇧', Cmd: '⌘' };
const MAC_KEY: Record<string, string> = { Enter: '↩', Escape: 'Esc' };

/**
 * A shortcut written in the menu bar's form (`CmdOrCtrl+Shift+S`, app/src-tauri/src/menu.rs) as the user sees it:
 * `⇧⌘S` on macOS and `Ctrl+Shift+S` elsewhere.
 */
export function keysText(accelerator: string, mac: boolean): string {
  const parts = accelerator.split('+');
  const key = parts.pop() ?? '';
  const mods = parts.map((m) => (m === 'CmdOrCtrl' ? (mac ? 'Cmd' : 'Ctrl') : m === 'Option' ? 'Alt' : m));
  if (!mac) return [...new Set(mods), key].join('+');
  const symbols = MAC_ORDER.filter((m) => mods.includes(m)).map((m) => MAC_SYMBOL[m]);
  return `${symbols.join('')}${MAC_KEY[key] ?? key}`;
}

/** A button's tooltip with its shortcut after it: `Close tab (⌘W)`. No shortcut leaves the label alone. */
export function withKeys(label: string, accelerator: string | null, mac: boolean = isMacPlatform()): string {
  return accelerator === null ? label : `${label} (${keysText(accelerator, mac)})`;
}

/**
 * The tooltip of a control that can be off for several reasons: the text of the first reason that holds, or
 * `undefined` (no tooltip) when none does. Busy states that the screen already shows need no reason.
 */
export function offReason(...reasons: [boolean, string][]): string | undefined {
  return reasons.find(([holds]) => holds)?.[1];
}

/**
 * Whether an element's text is cut short (an ellipsis): only then is its full text worth a tooltip, which would
 * otherwise repeat what is already readable.
 */
export function isCut(el: { scrollWidth: number; clientWidth: number }): boolean {
  return el.scrollWidth > el.clientWidth;
}

/**
 * The tooltip of a tab in the tab strip: its full title (the strip cuts long ones short) and, on a second line, the
 * connection and its environment, which the coloured top edge of the tab stands for. The connection's name is left
 * out when the title already says it.
 */
export function tabTip(title: string, connection: { name: string; environment: string } | null): string {
  if (connection === null) return title;
  const where = title.includes(connection.name) ? '' : `${connection.name} · `;
  return `${title}\n${where}${connection.environment} environment`;
}

/** The tooltip of a Live or Paused pill: what the dot means for the messages (or notifications) of a panel. */
export function liveTip(paused: boolean, what: string): string {
  return paused ? `Paused: new ${what} wait until you resume` : `New ${what} show as they arrive`;
}

const CORE_KEY_TYPES: Record<Exclude<KeyType, object>, string> = {
  string: 'String',
  list: 'List',
  set: 'Set',
  zset: 'Sorted set',
  hash: 'Hash',
  stream: 'Stream',
  json: 'JSON document',
  unknown: 'Type not read yet: it is read when you open the key',
};

/** The tooltip of a key's short type tag (STR, ZSET, TS, ...): the type's full name. */
export function keyTypeTip(type: KeyType): string {
  if (typeof type !== 'object') return CORE_KEY_TYPES[type];
  const module = moduleKindOf(type);
  if (module) return MODULE_KIND_LABELS[module];
  return type.other === 'ReJSON-RL' ? CORE_KEY_TYPES.json : `Module type ${type.other}`;
}

/** What each change kind of a server variable (`changeKind` in lib/live.ts) means, for the tooltip of its pill. */
export const CHANGE_KIND_TIPS: Readonly<Record<string, string>> = {
  'Read only': 'The server does not let this variable change',
  Restart: 'A new value takes effect after a server restart',
  Reload: 'A new value takes effect when the server reloads its configuration',
  'Not settable': 'Cannot be set from a session',
  Dynamic: 'A new value takes effect at once',
};
