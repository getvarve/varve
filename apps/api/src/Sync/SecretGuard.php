<?php

declare(strict_types=1);

namespace App\Sync;

/**
 * Defense in depth on secrets (design 8.8). The desktop app syncs profiles from a type without secret fields, and
 * passwords, passphrases and keys stay in each device's keychain (design 3.1 and 8.4). Should a client still send one,
 * the server refuses the payload instead of storing it.
 *
 * A field is a secret by its name: after dropping everything but letters and digits and lower-casing, the name is
 * `pwd` or ends in one of {@see self::SECRET_ENDINGS}. That covers the keychain's own kinds (`dbPassword`,
 * `ssh_key_passphrase`, `tlsKeyPassphrase`), `password`, `privateKey`, `client_secret`, `accessToken` and the like,
 * at any depth. Names that only talk about secrets stay allowed: `savedSecrets` (which kinds a device keeps in its
 * keychain), `secretKind`, `privateKeyPath`, `sslmode`. A secret-named field may still be null or true/false (a
 * "password saved" flag); any other value is refused.
 *
 * Any string value holding a PEM block (`-----BEGIN ...-----`, which includes OpenSSH private keys) or a PuTTY key
 * is refused too, whatever its field is called.
 */
final class SecretGuard
{
    public const array SECRET_ENDINGS = ['password', 'passwd', 'passphrase', 'privatekey', 'secret', 'token', 'apikey'];

    private const string PEM = '/-----BEGIN [A-Z0-9 ]{1,64}-----|PuTTY-User-Key-File-\d+:/';

    /**
     * The path of the first secret in a decoded payload (objects as \stdClass), with what is wrong; null when there is
     * none.
     *
     * @return array{field: string, message: string}|null
     */
    public static function find(mixed $value, string $path = 'payload'): ?array
    {
        if (\is_string($value)) {
            return 1 === preg_match(self::PEM, $value)
                ? ['field' => $path, 'message' => \sprintf('%s holds a PEM block or a private key. Keys and certificates stay on the device; sync the path to the file instead.', $path)]
                : null;
        }
        if (\is_array($value)) {
            foreach ($value as $index => $item) {
                $found = self::find($item, \sprintf('%s[%d]', $path, $index));
                if (null !== $found) {
                    return $found;
                }
            }

            return null;
        }
        if ($value instanceof \stdClass) {
            foreach (get_object_vars($value) as $name => $item) {
                $field = $path.'.'.$name;
                if (self::isSecretName((string) $name) && null !== $item && !\is_bool($item)) {
                    return ['field' => $field, 'message' => \sprintf('%s looks like a secret. Passwords, passphrases, keys and tokens never leave the device.', $field)];
                }
                $found = self::find($item, $field);
                if (null !== $found) {
                    return $found;
                }
            }
        }

        return null;
    }

    public static function isSecretName(string $name): bool
    {
        $normalized = strtolower((string) preg_replace('/[^A-Za-z0-9]/', '', $name));
        if ('pwd' === $normalized) {
            return true;
        }
        foreach (self::SECRET_ENDINGS as $ending) {
            if (str_ends_with($normalized, $ending)) {
                return true;
            }
        }

        return false;
    }
}
