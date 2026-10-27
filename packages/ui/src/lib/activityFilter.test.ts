import { parseActions } from './activityFilter';

describe('parseActions', () => {
  it('splits, trims, drops repeats and keeps prefix patterns', () => {
    expect(parseActions(' query.failed, connection.* query.failed ,, ')).toEqual(['query.failed', 'connection.*']);
  });

  it('ignores entries that cannot be actions', () => {
    expect(parseActions('Query Failed, *, query.executed')).toEqual(['query.executed']);
  });
});
