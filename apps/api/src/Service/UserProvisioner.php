<?php

declare(strict_types=1);

namespace App\Service;

use App\Entity\User;
use App\Organization\PersonalOrganizations;
use Doctrine\ORM\EntityManagerInterface;
use Psr\Clock\ClockInterface;
use Symfony\Component\PasswordHasher\Hasher\PasswordHasherFactoryInterface;

/**
 * Creates a user together with everything a new account needs: the personal organization it owns.
 */
final readonly class UserProvisioner
{
    public function __construct(
        private EntityManagerInterface $entityManager,
        private PasswordHasherFactoryInterface $passwordHashers,
        private PersonalOrganizations $personalOrganizations,
        private ClockInterface $clock,
    ) {
    }

    /**
     * @param bool $emailConfirmed whether the address is known to be the user's already; a sign-up confirms it later
     */
    public function provision(string $email, string $displayName, #[\SensitiveParameter] string $plainPassword, bool $emailConfirmed): User
    {
        return $this->provisionHashed($email, $displayName, $this->passwordHashers->getPasswordHasher(User::class)->hash($plainPassword), $emailConfirmed);
    }

    /** As {@see self::provision()}, for a caller that hashed the password already. */
    public function provisionHashed(string $email, string $displayName, #[\SensitiveParameter] string $passwordHash, bool $emailConfirmed): User
    {
        $now = $this->clock->now();
        $user = new User($email, $displayName, $now);
        $user->setPassword($passwordHash);
        if ($emailConfirmed) {
            $user->confirmEmail($now);
        }
        $this->entityManager->persist($user);
        $this->personalOrganizations->create($user);
        $this->entityManager->flush();

        return $user;
    }
}
