<?php

declare(strict_types=1);

namespace App\Enum;

/**
 * What a link in account mail is for, and how long it works.
 */
enum AccountLinkPurpose: string
{
    /** Proves the address of a new account. */
    case ConfirmEmail = 'confirm_email';
    /** Sets a new password for someone who forgot theirs; it proves the address too. */
    case ResetPassword = 'reset_password';

    public function lifetime(): \DateInterval
    {
        return match ($this) {
            self::ConfirmEmail => new \DateInterval('P2D'),
            self::ResetPassword => new \DateInterval('PT1H'),
        };
    }
}
