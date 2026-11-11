import { describe, expect, it, vi } from 'vitest';
import { ref } from 'vue';
import type { Router } from 'vue-router';
import { actionOpensTab, showWorkspace } from './showWorkspace';

function fakeRouter(name: string): { router: Router; push: ReturnType<typeof vi.fn> } {
  const push = vi.fn(async () => undefined);
  const router = { currentRoute: ref({ name }), push } as unknown as Router;
  return { router, push };
}

describe('showWorkspace', () => {
  it('goes to the workspace from Activity or Settings', () => {
    for (const page of ['activity', 'settings']) {
      const { router, push } = fakeRouter(page);
      showWorkspace(router);
      expect(push).toHaveBeenCalledWith({ name: 'home' });
    }
  });

  it('stays put on the workspace', () => {
    const { router, push } = fakeRouter('home');
    showWorkspace(router);
    expect(push).not.toHaveBeenCalled();
  });
});

describe('actionOpensTab', () => {
  it('is false only for actions that do not open a tab', () => {
    expect(actionOpensTab('refresh')).toBe(false);
    expect(actionOpensTab('copy-name')).toBe(false);
    expect(actionOpensTab('copy-qualified')).toBe(false);
    expect(actionOpensTab('data')).toBe(true);
    expect(actionOpensTab('new-query')).toBe(true);
    expect(actionOpensTab('new-kind:table')).toBe(true);
  });
});
