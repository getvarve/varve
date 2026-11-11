import { maskSecret, SECRET_MASK, shownStatements } from './variableSecrets';

describe('shownStatements', () => {
  it('masks the new value of a secret variable', () => {
    expect(shownStatements(["ALTER SYSTEM SET ssl_passphrase_command = 'echo s3cret'", 'SELECT pg_reload_conf()'], true)).toEqual([
      `ALTER SYSTEM SET ssl_passphrase_command = '${SECRET_MASK}'`,
      'SELECT pg_reload_conf()',
    ]);
    expect(shownStatements(["SET GLOBAL `wsrep_sst_auth` = 'root:a = b'"], true)).toEqual([`SET GLOBAL \`wsrep_sst_auth\` = '${SECRET_MASK}'`]);
  });

  it('keeps resets and other variables as planned', () => {
    expect(shownStatements(['SET GLOBAL report_password = DEFAULT'], true)).toEqual(['SET GLOBAL report_password = DEFAULT']);
    expect(shownStatements(['RESET PERSIST IF EXISTS report_password'], true)).toEqual(['RESET PERSIST IF EXISTS report_password']);
    expect(shownStatements(['SET GLOBAL max_connections = 200'], false)).toEqual(['SET GLOBAL max_connections = 200']);
  });
});

describe('maskSecret', () => {
  it('masks the refused value the server repeats in its message', () => {
    expect(maskSecret("Variable 'wsrep_sst_auth' can't be set to the value of 'root:pw'", 'root:pw')).toBe(
      `Variable 'wsrep_sst_auth' can't be set to the value of '${SECRET_MASK}'`,
    );
    expect(maskSecret('invalid value for parameter "ssl_passphrase_command": "echo s3cret" (echo s3cret)', 'echo s3cret')).toBe(
      `invalid value for parameter "ssl_passphrase_command": "${SECRET_MASK}" (${SECRET_MASK})`,
    );
    // Characters with a meaning in a pattern are taken as typed.
    expect(maskSecret("can't be set to the value of 'a.*(b)'", 'a.*(b)')).toBe(`can't be set to the value of '${SECRET_MASK}'`);
  });

  it('masks a very short value only where the server quotes it', () => {
    expect(maskSecret("Variable 'a_var' can't be set to the value of 'a'", 'a')).toBe(`Variable 'a_var' can't be set to the value of '${SECRET_MASK}'`);
    expect(maskSecret('invalid value for parameter "x": "ab"', 'ab')).toBe(`invalid value for parameter "x": "${SECRET_MASK}"`);
    expect(maskSecret('Access denied', '')).toBe('Access denied');
  });

  it('masks the start of a long value the server cut short', () => {
    const secret = 'correct horse battery staple and then some more words';
    expect(maskSecret(`can't be set to the value of '${secret.slice(0, 30)}'`, secret)).toBe(`can't be set to the value of '${SECRET_MASK}'`);
    // A short shared start is not a cut-off secret.
    expect(maskSecret('the correct value is unknown', secret)).toBe('the correct value is unknown');
  });
});
