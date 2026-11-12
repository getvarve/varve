<?php

declare(strict_types=1);

namespace App\Account;

/**
 * What a password must be: at least 12 characters, and at most the 4096 bytes the password hasher accepts.
 */
final class PasswordPolicy
{
    public const int MIN_LENGTH = 12;
    /** In bytes. */
    public const int MAX_LENGTH = 4096;

    /** What is wrong with a new password, or null when it will do. */
    public static function problem(#[\SensitiveParameter] string $password): ?string
    {
        if (mb_strlen($password) < self::MIN_LENGTH) {
            return \sprintf('Choose a password of at least %d characters.', self::MIN_LENGTH);
        }
        if (\strlen($password) > self::MAX_LENGTH) {
            return 'Choose a shorter password.';
        }

        return null;
    }
}
