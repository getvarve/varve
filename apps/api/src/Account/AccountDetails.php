<?php

declare(strict_types=1);

namespace App\Account;

use App\Entity\User;
use Symfony\Component\Validator\Constraints\Email;
use Symfony\Component\Validator\Validator\ValidatorInterface;

/**
 * What an account's email address and name must be, wherever one is made: the sign-up page and
 * `app:users:create`.
 */
final readonly class AccountDetails
{
    public function __construct(private ValidatorInterface $validator)
    {
    }

    /**
     * What is wrong with an email address, or null when it will do: at most {@see User::MAX_EMAIL} characters, an
     * address as browsers accept it, and one the mailer can send to (it follows the RFC more strictly, and refuses
     * for example two dots in a row).
     */
    public function emailProblem(string $email): ?string
    {
        if ('' === $email
            || mb_strlen($email) > User::MAX_EMAIL
            || \count($this->validator->validate($email, [new Email(mode: Email::VALIDATION_MODE_HTML5), new Email(mode: Email::VALIDATION_MODE_STRICT)])) > 0
        ) {
            return 'Enter a valid email address.';
        }

        return null;
    }

    /** What is wrong with a name, or null when it will do. */
    public static function nameProblem(string $name): ?string
    {
        if ('' === $name || mb_strlen($name) > User::MAX_DISPLAY_NAME || 1 === preg_match('/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/u', $name) || !mb_check_encoding($name, 'UTF-8')) {
            return \sprintf('Enter your name, at most %d characters.', User::MAX_DISPLAY_NAME);
        }

        return null;
    }
}
