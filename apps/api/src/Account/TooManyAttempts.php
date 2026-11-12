<?php

declare(strict_types=1);

namespace App\Account;

/**
 * A rate limit refused the request; it may be tried again at {@see self::$retryAfter}.
 */
final class TooManyAttempts extends \RuntimeException
{
    public function __construct(public readonly \DateTimeImmutable $retryAfter)
    {
        parent::__construct('Too many attempts. Try again later.');
    }
}
