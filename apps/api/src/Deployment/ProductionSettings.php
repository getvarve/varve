<?php

declare(strict_types=1);

namespace App\Deployment;

/**
 * Whether a production server's settings are safe to start with. The repository's .env files hold development values
 * that are public; a production image falls back to them when a setting is missing, so every secret has to be set,
 * long enough, and not one of those values.
 *
 * A new setting goes in the list below that fits it (a secret, an address, a switch), or gets a check of its own in
 * problems() when it needs more than that. ProductionSettingsTest::complete() holds a complete production
 * environment and gets the new setting too.
 */
final readonly class ProductionSettings
{
    /** Secrets with the minimum length each needs. */
    public const array SECRETS = [
        'APP_SECRET' => 32,
    ];

    /** Addresses that must be a bare origin: tokens and links are built on them. */
    public const array ORIGINS = ['API_PUBLIC_URL'];

    /** The desktop app's own origins (WebKit on macOS and Linux, WebView2 on Windows). */
    public const array DESKTOP_ORIGINS = ['tauri://localhost', 'http://tauri.localhost'];

    /**
     * Switches, read as on or off. A spelling Symfony's bool: processor does not know would quietly read as off, so
     * only these are allowed (and empty, which is off).
     */
    public const array SWITCHES = ['VARVE_BILLING'];

    /** Placeholder spellings the example settings use. */
    private const string PLACEHOLDER = '/change-?me/i';

    /**
     * @param array<string, string>       $settings    the environment the server runs with
     * @param array<string, list<string>> $development every value the repository's .env files give each setting
     */
    public function __construct(
        #[\SensitiveParameter]
        private array $settings,
        #[\SensitiveParameter]
        private array $development,
    ) {
    }

    /**
     * What is wrong, one line per problem; empty when the server may start. Never contains a secret's value.
     *
     * @return list<string>
     */
    public function problems(): array
    {
        $problems = [];
        foreach (self::SECRETS as $name => $length) {
            $problem = $this->secretProblem($name, $length);
            if (null !== $problem) {
                $problems[] = $problem;
            }
        }

        $problems = [...$problems, ...$this->originProblems(), ...$this->switchProblems()];

        $database = $this->value('DATABASE_URL');
        $password = rawurldecode((string) parse_url($database, \PHP_URL_PASS));
        if ('' === $database || 1 === preg_match(self::PLACEHOLDER, $database) || $this->isDevelopmentValue('DATABASE_URL', $database)) {
            $problems[] = 'DATABASE_URL is not set to your database.';
        } elseif (\strlen($password) < 16) {
            $problems[] = 'The database password in DATABASE_URL must be at least 16 characters.';
        }

        return $problems;
    }

    /** @return list<string> */
    private function originProblems(): array
    {
        $problems = [];
        foreach (self::ORIGINS as $name) {
            if (!self::isOrigin($this->value($name))) {
                $problems[] = \sprintf('%s must be an https address as a browser writes it (lower case, no path, trailing slash or :443), such as https://varve.example.com.', $name);
            }
        }

        $cors = $this->value('CORS_ALLOW_ORIGIN');
        if (str_starts_with($cors, '^')) {
            $problems[] = 'CORS_ALLOW_ORIGIN must list origins separated by spaces, not a regular expression.';
        } else {
            foreach (CorsOrigins::origins($cors) as $origin) {
                if (!\in_array($origin, self::DESKTOP_ORIGINS, true) && !self::isOrigin($origin)) {
                    $problems[] = \sprintf('CORS_ALLOW_ORIGIN lists "%s", which is not an https origin as a browser writes it, such as https://varve.example.com.', $origin);
                }
            }
        }

        return $problems;
    }

    /** @return list<string> */
    private function switchProblems(): array
    {
        $problems = [];
        foreach (self::SWITCHES as $name) {
            if (null === filter_var($this->value($name), \FILTER_VALIDATE_BOOL, \FILTER_NULL_ON_FAILURE)) {
                $problems[] = \sprintf('%s must be 0 or 1.', $name);
            }
        }

        return $problems;
    }

    /**
     * An https origin spelled the way a browser sends it in Origin, since the API compares it with that: a lower-case
     * host name (letters, digits, dots and hyphens; an international name in its xn-- form) and at most a port,
     * without a leading zero and never the default 443.
     */
    private static function isOrigin(string $value): bool
    {
        if (1 !== preg_match('{^https://[a-z0-9]([a-z0-9-]*[a-z0-9])?(\.[a-z0-9]([a-z0-9-]*[a-z0-9])?)*(:[1-9][0-9]{0,4})?\z}', $value)) {
            return false;
        }
        // false for a port above 65535.
        $port = parse_url($value, \PHP_URL_PORT);

        return null === $port || (\is_int($port) && 443 !== $port);
    }

    private function secretProblem(string $name, int $length): ?string
    {
        $value = $this->value($name);
        if ('' === $value) {
            return \sprintf('%s is not set.', $name);
        }
        if ($this->isDevelopmentValue($name, $value) || 1 === preg_match(self::PLACEHOLDER, $value)) {
            return \sprintf('%s is still a development or example value; generate a new one.', $name);
        }
        if (\strlen($value) < $length) {
            return \sprintf('%s must be at least %d characters.', $name, $length);
        }

        return null;
    }

    private function isDevelopmentValue(string $name, string $value): bool
    {
        return '' !== $value && \in_array($value, $this->development[$name] ?? [], true);
    }

    private function value(string $name): string
    {
        return trim($this->settings[$name] ?? '');
    }
}
