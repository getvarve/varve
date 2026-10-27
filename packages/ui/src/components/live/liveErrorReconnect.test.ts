// Every LiveError offers "Reconnect" for a reconnectRequired error; a panel
// that does not forward the event leaves that button without effect.
const sources = import.meta.glob<string>('../**/*.vue', { query: '?raw', import: 'default', eager: true });

describe('LiveError reconnect', () => {
  it('is forwarded by every panel that shows a LiveError', () => {
    const missing: string[] = [];
    for (const [path, source] of Object.entries(sources)) {
      for (const tag of source.match(/<LiveError\b[^>]*\/>/g) ?? []) {
        if (!tag.includes('@reconnect')) missing.push(`${path}: ${tag.replace(/\s+/g, ' ')}`);
      }
    }
    expect(Object.keys(sources).length).toBeGreaterThan(10);
    expect(missing).toEqual([]);
  });
});
