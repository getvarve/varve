<?php

declare(strict_types=1);

namespace App\Account;

/**
 * Who asks, for a rate limit: an IPv4 address (also when it arrives IPv4-mapped, `::ffff:a.b.c.d`) or an IPv6 /64,
 * since one network usually gets a whole /64.
 */
final class ClientAddress
{
    public static function key(?string $ip): string
    {
        if (null === $ip || false === filter_var($ip, \FILTER_VALIDATE_IP)) {
            return 'unknown';
        }
        $packed = inet_pton($ip);
        if (false === $packed) {
            return 'unknown';
        }
        if (16 === \strlen($packed) && str_starts_with($packed, str_repeat("\0", 10)."\xff\xff")) {
            $packed = substr($packed, 12);
        }

        return 4 === \strlen($packed)
            ? (string) inet_ntop($packed)
            : inet_ntop(substr($packed, 0, 8).str_repeat("\0", 8)).'/64';
    }
}
