import type { TlsMode } from '../api/records';
import { encryptionStatus, hostPort } from './display';

describe('hostPort', () => {
  it('puts IPv6 addresses in brackets', () => {
    expect(hostPort('::1', 22)).toBe('[::1]:22');
    expect(hostPort('[fe80::1]', 2222)).toBe('[fe80::1]:2222');
    expect(hostPort('bastion.acme.example', 22)).toBe('bastion.acme.example:22');
    expect(hostPort('10.0.0.5', 5432)).toBe('10.0.0.5:5432');
  });
});

describe('encryptionStatus', () => {
  const enc = (tls: boolean | null, mode: TlsMode, passwordUnencrypted = false) => ({ tls, mode, passwordUnencrypted });

  it('says whether the session is encrypted and whether the certificate is checked', () => {
    expect(encryptionStatus(enc(null, 'prefer'), false)).toBeNull();
    for (const mode of ['prefer', 'require'] as const) {
      expect(encryptionStatus(enc(true, mode), false)).toMatchObject({
        label: 'Encrypted (TLS, certificate not verified)',
        warn: false,
      });
    }
    expect(encryptionStatus(enc(true, 'verify_full'), false)).toMatchObject({ label: 'Encrypted (TLS)', warn: false });
    expect(encryptionStatus(enc(true, 'verify_ca'), true)?.hint).toContain('not the host name');
    expect(encryptionStatus(enc(false, 'disable', true), false)).toMatchObject({
      label: 'Not encrypted',
      note: null,
      warn: false,
    });
  });

  it('recommends require when prefer ended unencrypted', () => {
    const status = encryptionStatus(enc(false, 'prefer', true), false);
    expect(status).toMatchObject({ label: 'Not encrypted', note: 'use require', warn: true });
    expect(status?.hint).toContain('The password was sent unencrypted.');
    expect(status?.hint).toContain('Choose require');
    expect(encryptionStatus(enc(false, 'prefer'), false)?.hint).not.toContain('password');
  });

  it('separates the SSH leg from the leg to the server', () => {
    const status = encryptionStatus(enc(false, 'prefer', true), true);
    expect(status?.label).toBe('Not encrypted to the server (SSH to the bastion)');
    expect(status?.hint).toContain('SSH encrypts the way to the bastion');
    expect(status?.hint).toContain('sent unencrypted from the bastion to the server');
    expect(encryptionStatus(enc(false, 'disable'), true)?.label).toBe(
      'Not encrypted to the server (SSH to the bastion)',
    );
  });
});
