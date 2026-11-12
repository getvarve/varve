<?php

declare(strict_types=1);

namespace App\Security;

use App\Entity\User;
use Symfony\Component\Security\Core\Authentication\Token\TokenInterface;
use Symfony\Component\Security\Core\Exception\CustomUserMessageAccountStatusException;
use Symfony\Component\Security\Core\User\UserCheckerInterface;
use Symfony\Component\Security\Core\User\UserInterface;

/**
 * No sign-in before the address is confirmed. Checked after the password, so only someone who knows it learns that
 * the account waits for its confirmation; a wrong password reads as any other.
 */
final class ConfirmedEmailChecker implements UserCheckerInterface
{
    public const string UNCONFIRMED = 'Confirm your email address first: open the link in the mail we sent you.';

    public function checkPreAuth(UserInterface $user): void
    {
    }

    public function checkPostAuth(UserInterface $user, ?TokenInterface $token = null): void
    {
        if ($user instanceof User && !$user->isEmailConfirmed()) {
            throw new CustomUserMessageAccountStatusException(self::UNCONFIRMED);
        }
    }
}
