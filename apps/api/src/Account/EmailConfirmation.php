<?php

declare(strict_types=1);

namespace App\Account;

use App\Entity\User;
use App\Enum\AccountLinkPurpose;
use Doctrine\ORM\EntityManagerInterface;
use Psr\Clock\ClockInterface;
use Symfony\Component\PasswordHasher\Hasher\PasswordHasherFactoryInterface;

/**
 * Confirms the address of a new account with the link mailed to it. The link works once, for two days, and only while
 * it is the latest one sent.
 *
 * A link belongs to one sign-up and carries its name and password ({@see \App\Entity\AccountLink}). Confirming asks
 * for that password and then makes exactly those the account's. So the owner of a mailbox cannot, by opening a mail
 * they did not ask for, confirm an account with a password someone else chose: they do not know it.
 */
final readonly class EmailConfirmation
{
    public function __construct(
        private AccountLinks $links,
        private PasswordHasherFactoryInterface $passwordHashers,
        private EntityManagerInterface $entityManager,
        private ClockInterface $clock,
    ) {
    }

    /** Whether the link would still confirm an address. */
    public function isValid(#[\SensitiveParameter] string $secret): bool
    {
        $link = $this->links->find($secret, AccountLinkPurpose::ConfirmEmail);

        return null !== $link && null !== $link->getPendingPassword() && !$link->getUser()->isEmailConfirmed();
    }

    /** Whether the link is one this server sent that only expired: its holder may ask for a new one. */
    public function isExpired(#[\SensitiveParameter] string $secret): bool
    {
        $link = $this->links->known($secret, AccountLinkPurpose::ConfirmEmail);

        return null !== $link && !$link->getUser()->isEmailConfirmed() && !$this->isValid($secret);
    }

    /**
     * Confirms the address the link was sent to and gives the account the name and password of the link's sign-up,
     * when $password is that password.
     */
    public function confirm(#[\SensitiveParameter] string $secret, #[\SensitiveParameter] string $password): ConfirmationResult
    {
        $link = $this->links->find($secret, AccountLinkPurpose::ConfirmEmail);
        $user = $link?->getUser();
        $name = $link?->getPendingDisplayName();
        $passwordHash = $link?->getPendingPassword();
        if (null === $user || null === $name || null === $passwordHash || $user->isEmailConfirmed()) {
            return ConfirmationResult::LinkEnded;
        }
        if (!$this->passwordHashers->getPasswordHasher(User::class)->verify($passwordHash, $password)) {
            return ConfirmationResult::WrongPassword;
        }

        $this->entityManager->wrapInTransaction(function () use ($user, $name, $passwordHash): void {
            $user->setDisplayName($name);
            $user->setPassword($passwordHash);
            $user->confirmEmail($this->clock->now());
            $this->entityManager->flush();
            $this->links->end($user, AccountLinkPurpose::ConfirmEmail);
        });

        return ConfirmationResult::Confirmed;
    }
}
