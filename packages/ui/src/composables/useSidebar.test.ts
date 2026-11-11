import { beforeEach, describe, expect, it } from 'vitest';
import { clampSidebarWidth, SIDEBAR_IDEAL, SIDEBAR_MAX, SIDEBAR_MIN, useSidebar } from './useSidebar';

describe('useSidebar', () => {
  beforeEach(() => {
    localStorage.clear();
    useSidebar().reload();
  });

  it('starts shown at the ideal width', () => {
    const { state } = useSidebar();
    expect(state.collapsed).toBe(false);
    expect(state.width).toBe(SIDEBAR_IDEAL);
  });

  it('keeps the width inside its range', () => {
    expect(clampSidebarWidth(10)).toBe(SIDEBAR_MIN);
    expect(clampSidebarWidth(900)).toBe(SIDEBAR_MAX);
    expect(clampSidebarWidth(240.6)).toBe(241);
    expect(clampSidebarWidth(Number.NaN)).toBe(SIDEBAR_IDEAL);
  });

  it('remembers collapsing and the width', () => {
    const sidebar = useSidebar();
    sidebar.toggle();
    sidebar.setWidth(260);
    sidebar.reload();
    expect(sidebar.state.collapsed).toBe(true);
    expect(sidebar.state.width).toBe(260);
  });

  it('does not remember a width while it is being dragged', () => {
    const sidebar = useSidebar();
    sidebar.setWidth(280, false);
    expect(sidebar.state.width).toBe(280);
    sidebar.reload();
    expect(sidebar.state.width).toBe(SIDEBAR_IDEAL);
  });

  it('ignores a stored state of the wrong shape and clamps a stored width', () => {
    localStorage.setItem('varve.sidebar', JSON.stringify({ collapsed: 'yes', width: 200 }));
    const sidebar = useSidebar();
    sidebar.reload();
    expect(sidebar.state.collapsed).toBe(false);
    localStorage.setItem('varve.sidebar', JSON.stringify({ collapsed: true, width: 5000 }));
    sidebar.reload();
    expect(sidebar.state).toEqual({ collapsed: true, width: SIDEBAR_MAX });
  });

  it('goes back to the ideal width', () => {
    const sidebar = useSidebar();
    sidebar.setWidth(300);
    sidebar.resetWidth();
    sidebar.reload();
    expect(sidebar.state.width).toBe(SIDEBAR_IDEAL);
  });
});
