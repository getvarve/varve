<?php

declare(strict_types=1);

namespace App\Account;

use App\Device\Devices;
use App\Enum\AccountLinkPurpose;
use App\Repository\UserRepository;
use Doctrine\ORM\EntityManagerInterface;
use Psr\Clock\ClockInterface;
use Symfony\Component\Mailer\Exception\TransportExceptionInterface;
use Symfony\Component\PasswordHasher\Hasher\UserPasswordHasherInterface;

/**
 * A forgotten password: a link mailed to the address, which works once, for an hour, and only while it is the latest
 * one sent. Setting the new password also confirms the address (the link proves it) and signs the account out on
 * every device, since whoever knew the old password may be signed in somewhere; the browser's remember-me cookie ends
 * with the old password too.
 *
 * Asking for a link says the same whether or not the address has an account.
 */
final readonly class PasswordReset
{
    public function __construct(
        private UserRepository $users,
        private AccountLinks $links,
        private AccountMail $mail,
        private Devices $devices,
        private UserPasswordHasherInterface $passwordHasher,
        private EntityManagerInterface $entityManager,
        private ClockInterface $clock,
    ) {
    }

    /** @throws TransportExceptionInterface when the mail server refuses the mail */
    public function request(string $email, ?string $ip): void
    {
        $user = $this->users->findOneByEmail($email);
        if (null === $user || !$this->mail->mayMail($user->getEmail(), $ip)) {
            return;
        }
        $this->mail->passwordReset($user, $this->links->issue($user, AccountLinkPurpose::ResetPassword));
    }

    /** Whether the link would still set a password. */
    public function isValid(#[\SensitiveParameter] string $secret): bool
    {
        return null !== $this->links->find($secret, AccountLinkPurpose::ResetPassword);
    }

    /**
     * Sets the new password (which the caller checked against {@see PasswordPolicy}); false when the link does not
     * work (any more).
     */
    public function reset(#[\SensitiveParameter] string $secret, #[\SensitiveParameter] string $password): bool
    {
        $link = $this->links->find($secret, AccountLinkPurpose::ResetPassword);
        if (null === $link) {
            return false;
        }
        $user = $link->getUser();

        $this->entityManager->wrapInTransaction(function () use ($link, $user, $password): void {
            $user->setPassword($this->passwordHasher->hashPassword($user, $password));
            $user->confirmEmail($this->clock->now());
            $this->links->spend($link);
            $this->entityManager->flush();
            $this->devices->signOutEverywhere($user);
        });

        return true;
    }
}
