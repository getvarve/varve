<?php

declare(strict_types=1);

namespace App\Account;

use App\Device\Devices;
use App\Entity\User;
use App\Enum\AccountLinkPurpose;
use App\Repository\UserRepository;
use Doctrine\ORM\EntityManagerInterface;
use Psr\Clock\ClockInterface;
use Symfony\Component\PasswordHasher\Hasher\UserPasswordHasherInterface;

/**
 * A forgotten password: a link mailed to the address, which works once, for an hour, and only while it is the latest
 * one sent. Setting the new password also confirms the address (the link proves it) and signs the account out on
 * every device, since whoever knew the old password may be signed in somewhere; the browser's remember-me cookie ends
 * with the old password too.
 *
 * Asking for a link says the same, as fast, whether or not the address has an account: every request counts against
 * the same limit before the address is looked up, and the mail leaves after the response.
 *
 * Someone who asks from a browser signed in to that account, or with a reset link that expired, has shown the account
 * or the mailbox is theirs: their link is counted apart ({@see AccountMailBudget::ownerMayMail()}), so nobody who
 * keeps asking for links to their address can keep them from one.
 */
final readonly class PasswordReset
{
    public function __construct(
        private UserRepository $users,
        private AccountLinks $links,
        private AccountMail $mail,
        private AccountMailBudget $budget,
        private Devices $devices,
        private UserPasswordHasherInterface $passwordHasher,
        private EntityManagerInterface $entityManager,
        private ClockInterface $clock,
    ) {
    }

    /**
     * @param User|null $signedIn the account this browser is signed in to, if any
     */
    public function request(string $email, ?string $ip, ?User $signedIn = null): void
    {
        if (null !== $signedIn && $signedIn->getEmail() === mb_strtolower(trim($email))) {
            $this->requestByOwner($signedIn);

            return;
        }

        $mayAsk = $this->budget->requesterMayAsk(AccountMailBudget::PASSWORD_RESET, $ip);

        $user = $this->users->findOneByEmail($email);
        if (!$mayAsk || null === $user || !$this->budget->mayMail(AccountMailBudget::PASSWORD_RESET, $user->getEmail(), $ip)) {
            return;
        }
        $this->send($user);
    }

    /** A new link for someone who holds a reset link that expired; nothing for any other secret. */
    public function requestWithLink(#[\SensitiveParameter] string $secret): void
    {
        $link = $this->links->known($secret, AccountLinkPurpose::ResetPassword);
        if (null !== $link) {
            $this->requestByOwner($link->getUser());
        }
    }

    /** Whether the link would still set a password. */
    public function isValid(#[\SensitiveParameter] string $secret): bool
    {
        return null !== $this->links->find($secret, AccountLinkPurpose::ResetPassword);
    }

    /** Whether the link is one this server sent that only expired: its holder may ask for a new one. */
    public function isExpired(#[\SensitiveParameter] string $secret): bool
    {
        return !$this->isValid($secret) && null !== $this->links->known($secret, AccountLinkPurpose::ResetPassword);
    }

    /**
     * Sets the new password (which the caller checked against {@see PasswordPolicy}); false when the link does not
     * work (any more). Every link of the account ends with it: the reset links, and the confirmation links of
     * sign-ups, whose passwords must not become the account's any more.
     */
    public function reset(#[\SensitiveParameter] string $secret, #[\SensitiveParameter] string $password): bool
    {
        $link = $this->links->find($secret, AccountLinkPurpose::ResetPassword);
        if (null === $link) {
            return false;
        }
        $user = $link->getUser();

        $this->entityManager->wrapInTransaction(function () use ($user, $password): void {
            $user->setPassword($this->passwordHasher->hashPassword($user, $password));
            $user->confirmEmail($this->clock->now());
            $this->entityManager->flush();
            $this->links->end($user, AccountLinkPurpose::ResetPassword);
            $this->links->end($user, AccountLinkPurpose::ConfirmEmail);
            $this->devices->signOutEverywhere($user);
        });

        return true;
    }

    private function requestByOwner(User $user): void
    {
        if ($this->budget->ownerMayMail(AccountMailBudget::PASSWORD_RESET, $user)) {
            $this->send($user);
        }
    }

    private function send(User $user): void
    {
        $this->mail->passwordReset($user, $this->links->issue($user, AccountLinkPurpose::ResetPassword));
    }
}
