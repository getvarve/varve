import { anyDriverErrorOf } from '../api/errors';
import type { CmdError } from '../api/errors';

/** A refused init command of a Redis connection profile. */
export interface InitCommandProblem {
  /** 1-based line of the init text. */
  line: number;
  /** The reason, without the "Init command on line N:" prefix. */
  reason: string;
  /**
   * True when the line puts a password in the init text (`AUTH`,
   * `HELLO ... AUTH`): the password belongs in the credentials fields.
   */
  credentials: boolean;
}

const PREFIX = /^Init command on line (\d+): ([\s\S]*)$/;

/** The message an error carries, wherever the backend put it. */
function messageOf(error: CmdError): string | null {
  const driver = anyDriverErrorOf(error);
  if (driver) return driver.message;
  return 'message' in error && typeof error.message === 'string' ? error.message : null;
}

/**
 * The refused init command inside an error from saving a profile (`invalid`)
 * or from connecting (a driver error), or `null` for any other error.
 */
export function initCommandProblem(error: CmdError): InitCommandProblem | null {
  const message = messageOf(error);
  if (message === null) return null;
  const match = PREFIX.exec(message);
  if (!match) return null;
  const line = Number(match[1]);
  const reason = match[2] ?? '';
  if (!Number.isSafeInteger(line) || line < 1) return null;
  const word = reason.trimStart().split(/\s/, 1)[0]?.toUpperCase() ?? '';
  const credentials = (word === 'AUTH' || word === 'HELLO') && /puts a password/i.test(reason);
  return { line, reason, credentials };
}
