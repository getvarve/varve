<?php

declare(strict_types=1);

namespace App\Sync;

/**
 * Defense in depth on secrets (design 8.8). The desktop app syncs profiles from a type without secret fields, and
 * passwords, passphrases and keys stay in each device's keychain (design 3.1 and 8.4). Should a client still send one,
 * the server refuses the payload instead of storing it. The guard is for client bugs, not for a client that tries to
 * get around it: that client only stores its own user's secret.
 *
 * A field is a secret by its name ({@see self::isSecretName()}), at any depth. That covers the keychain's own kinds
 * (`dbPassword`, `ssh_key_passphrase`, `tlsKeyPassphrase`), `password`, `privateKey`, `client_secret`, `accessToken`,
 * `connectionString` and the like. Names that only talk about secrets stay allowed: `savedSecrets` (which kinds a
 * device keeps in its keychain), `secretKind`, `privateKeyPath`, `keyPath`, `sslmode`, and `auth` (how the SSH tunnel
 * signs in). A secret-named field may still be null or true/false (a "password saved" flag); any other value is
 * refused.
 *
 * A name and value pair is checked like a field: an object with a `name` or `key` that is a secret name and a `value`
 * that is not null or true/false, as in a list of driver parameters.
 *
 * A string is a secret by what it holds, whatever its field is called:
 * - private key material: a PEM private key block (OpenSSH and PGP private keys included) or a PuTTY key. Certificates
 *   and public keys are not secrets and pass, so SQL that holds one can sync;
 * - a value that as a whole is an address or connection string with a password in it: `scheme://user:password@host`,
 *   `user:password@host`, a URL with a `password=` parameter, or `host=db user=kim password=...`. Only a value that
 *   starts as one counts, so SQL that mentions a URL or a password is left alone;
 * - JSON in a string: when a string is itself a JSON object or list, what it holds is checked by the same rules.
 */
final class SecretGuard
{
    /** A name that, without digits at its end and a closing "value", ends in one of these is a secret. */
    public const array SECRET_ENDINGS = [
        'password', 'passwords', 'passwd', 'pwd', 'passphrase', 'passphrases', 'passcode',
        'privatekey', 'privatekeys', 'secretkey', 'accesskey', 'sshkey', 'encryptionkey', 'signingkey', 'apikey', 'apikeys',
        'secret', 'token', 'tokens', 'credential', 'credentials', 'authorization', 'connectionstring', 'dsn',
    ];

    /** Names that are a secret only as the whole name: as an ending they are parts of harmless words. */
    public const array SECRET_NAMES = ['pass', 'pw', 'key', 'pin', 'bearer', 'cookie'];

    private const string PRIVATE_KEY = '/-----BEGIN (?:[A-Z0-9 ]{0,48} )?PRIVATE KEY(?: BLOCK)?-----|PuTTY-User-Key-File-\d+:/';

    /** scheme://user:password@, also jdbc:scheme://, at the start of the value. */
    private const string URL_WITH_PASSWORD = '~^\s*(?:[a-z][a-z0-9+.-]*:)+//[^/\s:@]*:[^/\s@]+@~i';

    /** user:password@host as the whole value. */
    private const string HOST_WITH_PASSWORD = '~^\s*[^/\s:@]+:[^/\s@]+@[^/\s:@]+(?::\d+)?(?:/\S*)?\s*$~';

    /** A URL with a password among its parameters. */
    private const string URL_WITH_PASSWORD_PARAMETER = '~^\s*(?:[a-z][a-z0-9+.-]*:)+//\S*[?&;](?:password|passwd|pwd|pass|sslpassword|secret|token|access_token|api_key|apikey)=[^&;\s]~i';

    /** A whole value of `key=value` pairs, separated by spaces or semicolons: libpq, ODBC and ADO.NET connection strings. */
    private const string KEY_VALUE_PAIRS = '~^\s*(?:[a-z_][a-z0-9_ .-]*\s*=\s*(?:\'[^\']*\'|"[^"]*"|\{[^}]*\}|[^;\s\'"]*)(?:[;\s]+|$)){2,}$~i';

    /** The password among such pairs. */
    private const string PASSWORD_PAIR = '~(?:^|[;\s])(?:password|passwd|pwd|pass|sslpassword)\s*=\s*[^;\s]~i';

    /** What such pairs connect to: without one of these they are not a connection string (`SET a = 1; SET b = 2`). */
    private const string ADDRESS_PAIR = '~(?:^|[;\s])(?:host|hostaddr|server|data source|addr|address|dbname|database|initial catalog|user|username|uid|user id|port)\s*=~i';

    /**
     * The path of the first secret in a decoded payload (objects as \stdClass), with what is wrong; null when there is
     * none.
     *
     * @return array{field: string, message: string}|null
     */
    public static function find(mixed $value, string $path = 'payload'): ?array
    {
        if (\is_string($value)) {
            return self::inText($value, $path);
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
        if (!$value instanceof \stdClass) {
            return null;
        }

        $fields = get_object_vars($value);
        // A name and value pair: its `key` is a name here, not a field that could hold a key.
        $pair = \array_key_exists('value', $fields);
        if ($pair && self::holdsSecret($fields['value'])) {
            foreach (['name', 'key'] as $label) {
                if (\is_string($fields[$label] ?? null) && self::isSecretName($fields[$label])) {
                    return self::secret($path.'.value');
                }
            }
        }
        foreach ($fields as $name => $item) {
            $field = $path.'.'.$name;
            if (!($pair && 'key' === $name) && self::isSecretName((string) $name) && self::holdsSecret($item)) {
                return self::secret($field);
            }
            $found = self::find($item, $field);
            if (null !== $found) {
                return $found;
            }
        }

        return null;
    }

    /**
     * Whether a field of this name would hold a secret. Everything but letters and digits is dropped and the rest
     * lower-cased, then digits at the end and one closing "value" go (`password2`, `tokenValue`); what is left is a
     * secret when it is one of {@see self::SECRET_NAMES} or ends in one of {@see self::SECRET_ENDINGS}.
     */
    public static function isSecretName(string $name): bool
    {
        $normalized = rtrim(strtolower((string) preg_replace('/[^A-Za-z0-9]/', '', $name)), '0123456789');
        if (str_ends_with($normalized, 'value')) {
            $normalized = rtrim(substr($normalized, 0, -5), '0123456789');
        }
        if (\in_array($normalized, self::SECRET_NAMES, true)) {
            return true;
        }
        foreach (self::SECRET_ENDINGS as $ending) {
            if (str_ends_with($normalized, $ending)) {
                return true;
            }
        }

        return false;
    }

    /** A secret-named field may be empty or a flag; anything else in it is the secret. */
    private static function holdsSecret(mixed $value): bool
    {
        return null !== $value && !\is_bool($value);
    }

    /**
     * @return array{field: string, message: string}|null
     */
    private static function inText(string $text, string $path): ?array
    {
        $short = Rejection::path($path);
        if (1 === preg_match(self::PRIVATE_KEY, $text)) {
            return ['field' => $short, 'message' => \sprintf('%s holds a private key. Keys stay on the device; sync the path to the key file instead.', $short)];
        }
        if (1 === preg_match(self::URL_WITH_PASSWORD, $text)
            || 1 === preg_match(self::HOST_WITH_PASSWORD, $text)
            || 1 === preg_match(self::URL_WITH_PASSWORD_PARAMETER, $text)
            || (1 === preg_match(self::PASSWORD_PAIR, $text) && 1 === preg_match(self::ADDRESS_PAIR, $text) && 1 === preg_match(self::KEY_VALUE_PAIRS, $text))) {
            return ['field' => $short, 'message' => \sprintf('%s holds an address or connection string with a password in it. Passwords never leave the device; sync the address without it.', $short)];
        }

        $start = ltrim($text)[0] ?? '';
        if ('{' !== $start && '[' !== $start) {
            return null;
        }
        try {
            $decoded = json_decode($text, false, 512, \JSON_THROW_ON_ERROR);
        } catch (\JsonException) {
            return null;
        }
        $inside = \is_array($decoded) || $decoded instanceof \stdClass ? self::find($decoded, $path) : null;

        return null === $inside ? null : ['field' => $short, 'message' => \sprintf('%s holds JSON with a secret in it. %s', $short, $inside['message'])];
    }

    /** @return array{field: string, message: string} */
    private static function secret(string $path): array
    {
        $short = Rejection::path($path);

        return ['field' => $short, 'message' => \sprintf('%s looks like a secret. Passwords, passphrases, keys and tokens never leave the device.', $short)];
    }
}
