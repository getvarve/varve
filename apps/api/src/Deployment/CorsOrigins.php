<?php

declare(strict_types=1);

namespace App\Deployment;

use Symfony\Component\DependencyInjection\EnvVarProcessorInterface;

/**
 * Reads CORS_ALLOW_ORIGIN for the CORS bundle (`%env(cors_origins:CORS_ALLOW_ORIGIN)%`). A production server lists the
 * origins it allows, separated by spaces, and gets an anchored pattern of exactly those, so nobody has to write a
 * regular expression (where one unescaped dot lets another site in). A value that starts with ^ is already a pattern,
 * as in development, where any localhost port is allowed.
 */
final class CorsOrigins implements EnvVarProcessorInterface
{
    public function getEnv(string $prefix, string $name, \Closure $getEnv): string
    {
        $value = $getEnv($name);

        return self::pattern(\is_string($value) ? trim($value) : '');
    }

    public static function pattern(string $value): string
    {
        if (str_starts_with($value, '^')) {
            return $value;
        }

        $origins = self::origins($value);
        if ([] === $origins) {
            // Matches no origin at all: an empty list allows none rather than every site.
            return '^(?!)\z';
        }

        // \z, not $: a $ would also match before a trailing newline.
        return '^('.implode('|', array_map(static fn (string $origin): string => preg_quote($origin, '{'), $origins)).')\z';
    }

    /** @return list<string> */
    public static function origins(string $value): array
    {
        return array_values(array_filter(preg_split('/\s+/', trim($value)) ?: [], static fn (string $origin): bool => '' !== $origin));
    }

    public static function getProvidedTypes(): array
    {
        return ['cors_origins' => 'string'];
    }
}
