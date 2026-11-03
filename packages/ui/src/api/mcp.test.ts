import { claudeHttpCommand, claudeStdioCommand, shellQuote } from './mcp';
import type { McpStatus } from './mcp';

const status = (overrides: Partial<McpStatus> = {}): McpStatus => ({
  settings: { enabled: true, port: 7465 },
  url: 'http://127.0.0.1:7465/mcp',
  error: null,
  tokenFile: '/Users/me/Library/Application Support/Varve/mcp-token',
  bridgeCommand: ['/Applications/Varve.app/Contents/MacOS/varve', 'mcp'],
  ...overrides,
});

describe('shellQuote', () => {
  it('leaves plain words alone and quotes the rest', () => {
    expect(shellQuote('/usr/bin/varve')).toBe('/usr/bin/varve');
    expect(shellQuote('/Users/me/Application Support')).toBe("'/Users/me/Application Support'");
    expect(shellQuote("it's")).toBe("'it'\\''s'");
  });
});

describe('claude mcp add commands', () => {
  it('starts the bridge over stdio', () => {
    expect(claudeStdioCommand(status())).toBe(
      'claude mcp add --scope user varve -- /Applications/Varve.app/Contents/MacOS/varve mcp',
    );
    expect(claudeStdioCommand(status({ bridgeCommand: [] }))).toBeNull();
  });

  it('reads the token from its file for HTTP, never inlining it', () => {
    expect(claudeHttpCommand(status())).toBe(
      'claude mcp add --scope user --transport http varve http://127.0.0.1:7465/mcp --header ' +
        `"Authorization: Bearer $(cat '/Users/me/Library/Application Support/Varve/mcp-token')"`,
    );
    expect(claudeHttpCommand(status({ url: null }))).toBeNull();
  });
});
