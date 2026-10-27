import type { DropImpact } from '../api/ddl';
import { appTabsNote, contentsText, dependentsNote, impactHeading, restrictFails, sessionsNote } from './dropImpact';

function impact(extra: Partial<DropImpact> = {}): DropImpact {
  return { dependents: [], dependentsTruncated: false, contents: [], sessions: [], sessionsComplete: true, blockers: [], appSessions: [], notes: [], ...extra };
}

const session = { id: '4242', user: 'app', application: 'psql', client: '10.0.0.5', state: 'idle', varve: false };

describe('drop impact texts', () => {
  it('counts contents in words', () => {
    expect(contentsText([{ label: 'tables', count: '3' }, { label: 'indexes', count: '1' }, { label: 'policies', count: '1' }])).toBe('3 tables, 1 index, 1 policy');
    expect(contentsText([{ label: 'materialized views', count: '1' }])).toBe('1 materialized view');
    // Counts travel as decimal text and are shown as they are, however large.
    expect(contentsText([{ label: 'tables', count: '90071992547409930' }])).toBe('90071992547409930 tables');
  });

  it('explains dependents per engine and CASCADE', () => {
    const deps = impact({ dependents: [{ description: 'view app.v', effect: 'blocks' }], dependentsTruncated: true });
    expect(dependentsNote('postgres', deps, false)).toMatch(/fails unless you choose CASCADE/);
    expect(dependentsNote('postgres', deps, true)).toMatch(/CASCADE also drops 1 object .*\(and more\)/);
    expect(restrictFails('postgres', deps, false)).toBe(true);
    expect(restrictFails('postgres', deps, true)).toBe(false);
    const my = impact({ dependents: [{ description: 'foreign key fk on table d.t', effect: 'blocks' }, { description: 'view d.v', effect: 'breaks' }] });
    expect(dependentsNote('mysql', my, false)).toMatch(/refuses the drop.*1 view uses it/);
    expect(restrictFails('mariadb', my, true)).toBe(true);
    expect(dependentsNote('postgres', impact(), false)).toBeNull();
  });

  it('explains connected sessions per engine and FORCE', () => {
    const pg = impact({ sessions: [session, { ...session, id: '7', application: 'Varve', varve: true }] });
    expect(sessionsNote('postgres', pg, false)).toMatch(/refuses to drop .*2 sessions are connected.*1 of them is a Varve session/);
    expect(sessionsNote('postgres', pg, true)).toMatch(/WITH \(FORCE\) terminates the 2 other sessions/);
    expect(sessionsNote('postgres', impact(), false)).toMatch(/No other session/);
    const my = impact({ sessions: [session], sessionsComplete: false });
    expect(sessionsNote('mysql', my, false)).toMatch(/do not refuse.*lock_wait_timeout.*PROCESS/);
    expect(sessionsNote('mariadb', impact(), false)).toMatch(/No other connection/);
  });

  it('says routines, triggers and events may break', () => {
    const my = impact({ dependents: [{ description: 'procedure crm.monthly', effect: 'mayBreak' }, { description: 'trigger crm.t_ai on table crm.t', effect: 'mayBreak' }] });
    expect(dependentsNote('mysql', my, false)).toMatch(/2 routines, triggers or events name it: they may fail/);
    expect(restrictFails('mysql', my, false)).toBe(false);
  });

  it('names the tabs a database drop ends or leaves without their database', () => {
    const tabs = [
      { title: 'Shop', busy: false, openTransaction: true, certain: true, standby: false },
      { title: 'Shop (2)', busy: false, openTransaction: false, certain: true, standby: false },
    ];
    expect(appTabsNote('postgres', tabs, true)).toMatch(/also ends 2 tabs .*"Shop" \(open transaction\), "Shop \(2\)".*rolled back/);
    expect(appTabsNote('postgres', tabs, false)).toMatch(/refuses the drop until they are closed/);
    expect(appTabsNote('mysql', [{ title: 'Shop', busy: true, openTransaction: null, certain: true, standby: false }], false)).toMatch(/"Shop" \(running a statement\).*wait/);
    expect(appTabsNote('postgres', [], true)).toBeNull();
    expect(appTabsNote('postgres', tabs, true)).toMatch(/same server and a copy of it .* cannot be told apart\.$/);
    // A tab whose server could not be identified is never asserted, only named as possible.
    const maybe = { title: 'Other server?', busy: false, openTransaction: null, certain: false, standby: false };
    expect(appTabsNote('postgres', [tabs[0] as (typeof tabs)[number], maybe], true)).toMatch(/ends 1 tab .*"Shop".*May also include "Other server\?": its server could not be identified/);
    expect(appTabsNote('postgres', [maybe], false)).toMatch(/^No tab in this app is known to use this database on this server\. May also include/);
  });

  it('describes standby tabs as ended by the replayed drop, never as blockers', () => {
    const standby = { title: 'Replica', busy: false, openTransaction: true, certain: true, standby: true };
    const note = appTabsNote('postgres', [standby], false) ?? '';
    expect(note).not.toMatch(/refuses/);
    expect(note).toMatch(/On a standby of this server: "Replica" \(open transaction\)\. It is ended when the drop replays on the standby, with or without FORCE.*does not stop the drop/);
    const primary = { title: 'Shop', busy: false, openTransaction: false, certain: true, standby: false };
    expect(appTabsNote('postgres', [primary, standby], false)).toMatch(/1 tab in this app uses the database, so PostgreSQL refuses .*"Shop"\. On a standby/);
  });

  it('names the panel by kind', () => {
    expect(impactHeading('database')).toMatch(/affects/);
    expect(impactHeading('schema')).toMatch(/schema/);
    expect(impactHeading('table')).toMatch(/depends/);
  });
});
