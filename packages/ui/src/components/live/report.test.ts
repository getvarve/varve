import type * as TabsModule from '../../composables/useTabs';
import { useReporter } from './report';

const reportTabError = vi.fn((_id: string, error: unknown, _sessionId?: string) => ({ kind: 'notFound' as const, message: String(error) }));

vi.mock('../../composables/useTabs', async (original) => ({
  ...(await original<typeof TabsModule>()),
  useTabs: () => ({ reportTabError }),
}));

describe('useReporter', () => {
  beforeEach(() => {
    reportTabError.mockClear();
  });

  it('reports a failed call to the tab with the session it was sent on', () => {
    const report = useReporter(() => 'tab-1');
    report(new Error('gone'), 's1');
    expect(reportTabError).toHaveBeenCalledTimes(1);
    expect(reportTabError.mock.calls[0]?.[0]).toBe('tab-1');
    expect(reportTabError.mock.calls[0]?.[2]).toBe('s1');
  });

  it('only parses a failure that is not a call on the session, and any failure outside a tab', () => {
    const inTab = useReporter(() => 'tab-1');
    expect(inTab(new Error('The clipboard is not available.'), null).kind).toBeDefined();
    const outside = useReporter(() => null);
    expect(outside(new Error('gone'), 's1').kind).toBeDefined();
    expect(reportTabError).not.toHaveBeenCalled();
  });
});
