import { retentionShortened, activityFilterSchema, deleteFolderFormSchema, destructiveFormSchema, hostKeyFormSchema, pickerFormSchema, moveFormSchema, activitySettingsForm, activitySettingsFrom, activitySettingsSchema, renameFormSchema, scanFormSchema, secretFormSchema } from './forms';
import { isRequired } from './schemaRequired';

const messages = (result: { success: boolean; error?: { issues: { path: (string | number)[]; message: string }[] } }) =>
  Object.fromEntries((result.error?.issues ?? []).map((i) => [i.path.join('.'), i.message]));

describe('small form schemas', () => {
  it('secret prompt requires a value and names the secret', () => {
    const schema = secretFormSchema('ssh_key_passphrase');
    expect(messages(schema.safeParse({ value: '', save: true }))).toEqual({ value: 'Enter the SSH key passphrase.' });
    expect(schema.safeParse({ value: 'x', save: false }).success).toBe(true);
    expect(isRequired(schema, { value: 'x', save: true }, 'value')).toBe(true);
  });

  it('destructive confirmation needs the exact name on production only', () => {
    const prod = destructiveFormSchema('production', 'Orders primary');
    expect(prod.safeParse({ typed: '' }).success).toBe(false);
    expect(messages(prod.safeParse({ typed: 'Orders' }))).toEqual({ typed: 'The name does not match this connection.' });
    expect(prod.safeParse({ typed: 'Orders primary' }).success).toBe(true);
    expect(isRequired(prod, { typed: 'Orders primary' }, 'typed')).toBe(true);
    const staging = destructiveFormSchema('staging', 'Orders staging');
    expect(staging.safeParse({ typed: '' }).success).toBe(true);
    expect(isRequired(staging, { typed: '' }, 'typed')).toBe(false);
  });

  it('scan form checks pattern, delimiter and COUNT', () => {
    expect(scanFormSchema.safeParse({ pattern: '*', typeFilter: '', delimiter: ':', count: 1000 }).success).toBe(true);
    expect(Object.keys(messages(scanFormSchema.safeParse({ pattern: ' ', typeFilter: 'hash', delimiter: '123456789', count: 7 })))).toEqual([
      'pattern',
      'delimiter',
      'count',
    ]);
  });

  it('rename requires 1 to 200 characters', () => {
    expect(renameFormSchema.safeParse({ name: '  ' }).success).toBe(false);
    expect(renameFormSchema.safeParse({ name: 'x'.repeat(201) }).success).toBe(false);
    expect(renameFormSchema.safeParse({ name: 'Webshop' }).success).toBe(true);
  });

  it('move only allows the offered folders or the top level', () => {
    const schema = moveFormSchema(['a', 'b']);
    expect(schema.safeParse({ folderId: '' }).success).toBe(true);
    expect(schema.safeParse({ folderId: 'b' }).success).toBe(true);
    expect(schema.safeParse({ folderId: 'self' }).success).toBe(false);
  });
});

describe('activity and settings forms', () => {
  const base = { text: '', outcome: '' as const, connectionId: '', actions: '' };
  it('accepts action names and prefixes, rejects anything else with a message', () => {
    expect(activityFilterSchema.safeParse({ ...base, actions: 'query.failed, connection.*' }).success).toBe(true);
    expect(messages(activityFilterSchema.safeParse({ ...base, actions: 'query.failed, Query Failed' }))).toEqual({
      actions: 'Not actions: Query, Failed. Use names such as query.failed, or connection.* for a prefix.',
    });
  });

  it('validates the retention period like the backend (1 day to 100 years, or forever)', () => {
    const base = { textLevel: 'normalized' as const, keep: 'period' as const, amount: '90', unit: 'days' as const };
    expect(activitySettingsSchema.safeParse(base).success).toBe(true);
    expect(messages(activitySettingsSchema.safeParse({ ...base, amount: '' }))).toEqual({ amount: 'Enter how long to keep events.' });
    expect(activitySettingsSchema.safeParse({ ...base, amount: '0' }).success).toBe(false);
    expect(activitySettingsSchema.safeParse({ ...base, amount: '1.5' }).success).toBe(false);
    expect(activitySettingsSchema.safeParse({ ...base, amount: '100', unit: 'years' }).success).toBe(true);
    expect(activitySettingsSchema.safeParse({ ...base, amount: '101', unit: 'years' }).success).toBe(false);
    expect(activitySettingsSchema.safeParse({ ...base, keep: 'forever', amount: '' }).success).toBe(true);
    expect(isRequired(activitySettingsSchema, base, 'amount')).toBe(true);
    expect(isRequired(activitySettingsSchema, { ...base, keep: 'forever' }, 'amount')).toBe(false);
  });

  it('converts between stored days and the form', () => {
    expect(activitySettingsForm({ textLevel: 'full', retentionDays: null })).toMatchObject({ keep: 'forever' });
    expect(activitySettingsForm({ textLevel: 'none', retentionDays: 730 })).toEqual({ textLevel: 'none', keep: 'period', amount: '2', unit: 'years' });
    expect(activitySettingsForm({ textLevel: 'none', retentionDays: 90 })).toEqual({ textLevel: 'none', keep: 'period', amount: '3', unit: 'months' });
    expect(activitySettingsForm({ textLevel: 'none', retentionDays: 45 })).toEqual({ textLevel: 'none', keep: 'period', amount: '45', unit: 'days' });
    expect(activitySettingsFrom({ textLevel: 'normalized', keep: 'period', amount: '6', unit: 'months' })).toEqual({ textLevel: 'normalized', retentionDays: 180 });
    expect(activitySettingsFrom({ textLevel: 'full', keep: 'forever', amount: '6', unit: 'months' })).toEqual({ textLevel: 'full', retentionDays: null });
  });
});

describe('dialog forms', () => {
  it('only deletes contents of a folder that has some', () => {
    expect(deleteFolderFormSchema(0).safeParse({ mode: 'delete_contents' }).success).toBe(false);
    expect(deleteFolderFormSchema(0).safeParse({ mode: 'move_contents_to_parent' }).success).toBe(true);
    expect(deleteFolderFormSchema(3).safeParse({ mode: 'delete_contents' }).success).toBe(true);
  });

  it('requires the acknowledgement only for a changed host key', () => {
    expect(hostKeyFormSchema(true).safeParse({ acknowledged: false }).success).toBe(false);
    expect(hostKeyFormSchema(true).safeParse({ acknowledged: true }).success).toBe(true);
    expect(hostKeyFormSchema(false).safeParse({ acknowledged: false }).success).toBe(true);
    expect(isRequired(hostKeyFormSchema(true), { acknowledged: true }, 'acknowledged')).toBe(true);
    expect(isRequired(hostKeyFormSchema(false), { acknowledged: true }, 'acknowledged')).toBe(false);
    // An ordinary checkbox or a number is never marked as required.
    expect(isRequired(secretFormSchema('db_password'), { value: 'x', save: true }, 'save')).toBe(false);
    expect(isRequired(scanFormSchema, { pattern: '*', typeFilter: '', delimiter: ':', count: 1000 }, 'count')).toBe(false);
  });

  it('caps the picker filter', () => {
    expect(pickerFormSchema.safeParse({ filter: 'x'.repeat(201) }).success).toBe(false);
  });
});

describe('retentionShortened', () => {
  it('is true only when events kept before would now be deleted', () => {
    expect(retentionShortened(null, 90)).toBe(true);
    expect(retentionShortened(365, 90)).toBe(true);
    expect(retentionShortened(90, 365)).toBe(false);
    expect(retentionShortened(90, null)).toBe(false);
    expect(retentionShortened(90, 90)).toBe(false);
  });
});
