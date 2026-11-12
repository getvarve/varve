<?php

declare(strict_types=1);

namespace App\Account;

use App\Entity\User;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\PasswordHasher\Hasher\UserPasswordHasherInterface;
use Symfony\Component\RateLimiter\RateLimiterFactoryInterface;

/**
 * Checks the user's current password before something that needs it (a new password, deleting the account). At most
 * 5 checks per user in 15 minutes, right or wrong, so a stolen access token is no way to guess the password.
 */
final readonly class PasswordCheck
{
    public function __construct(
        private UserPasswordHasherInterface $passwordHasher,
        #[Autowire(service: 'limiter.password_check')]
        private RateLimiterFactoryInterface $checks,
    ) {
    }

    /** @throws TooManyAttempts when the user checked too often */
    public function isCurrent(User $user, #[\SensitiveParameter] string $password): bool
    {
        $limit = $this->checks->create($user->getId()->toRfc4122())->consume();
        if (!$limit->isAccepted()) {
            throw new TooManyAttempts($limit->getRetryAfter());
        }

        return $this->passwordHasher->isPasswordValid($user, $password);
    }
}
