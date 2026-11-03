import { call } from './invoke';

/** `RECORDS_CHANGED_EVENT` in app/src-tauri/src/mcp: an MCP tool changed folders, connections or saved queries. */
export const RECORDS_CHANGED = 'varve://records-changed';
/** `SETTINGS_CHANGED_EVENT` in app/src-tauri/src/mcp: an MCP tool changed settings. */
export const SETTINGS_CHANGED = 'varve://settings-changed';

/** `McpSettings` in app/src-tauri/src/settings.rs. */
export interface McpSettings {
  /** Serve MCP on 127.0.0.1 while the app runs. Off by default. */
  enabled: boolean;
  /** TCP port, 1024 to 65535. */
  port: number;
}

/** `McpStatus` in app/src-tauri/src/mcp. */
export interface McpStatus {
  settings: McpSettings;
  /** The URL clients connect to while the server runs. */
  url: string | null;
  /** Why the server is not running although it is enabled. */
  error: string | null;
  /** Path of the file with the bearer token. */
  tokenFile: string;
  /** The command that starts the stdio bridge (this executable and `mcp`); empty when unknown. */
  bridgeCommand: string[];
}

export function fetchMcpStatus(): Promise<McpStatus> {
  return call<McpStatus>('mcp_status');
}

/** Stores the settings and starts, stops or moves the server to match. */
export function saveMcpSettings(settings: McpSettings): Promise<McpStatus> {
  return call<McpStatus>('set_mcp_settings', { settings });
}

/** Writes a new token; clients with the old one are refused from their next request on. */
export function rotateMcpToken(): Promise<McpStatus> {
  return call<McpStatus>('rotate_mcp_token');
}

/** Quotes one argument for a POSIX shell (and PowerShell, which reads single quotes the same way). */
export function shellQuote(value: string): string {
  return /^[\w@%+=:,./-]+$/.test(value) ? value : `'${value.replace(/'/g, `'\\''`)}'`;
}

/** `claude mcp add` for the stdio bridge, or null when the executable is unknown. */
export function claudeStdioCommand(status: McpStatus): string | null {
  if (status.bridgeCommand.length === 0) return null;
  return `claude mcp add --scope user varve -- ${status.bridgeCommand.map(shellQuote).join(' ')}`;
}

/** `claude mcp add` for Streamable HTTP, reading the token from its file; null while the server is off. */
export function claudeHttpCommand(status: McpStatus): string | null {
  if (!status.url) return null;
  const header = `"Authorization: Bearer $(cat ${shellQuote(status.tokenFile)})"`;
  return `claude mcp add --scope user --transport http varve ${status.url} --header ${header}`;
}
