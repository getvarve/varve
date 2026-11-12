<?php

declare(strict_types=1);

namespace App\Account;

use App\Entity\User;
use App\Enum\AccountLinkPurpose;
use App\Repository\UserRepository;
use App\Service\UserProvisioner;
use Doctrine\DBAL\Exception\UniqueConstraintViolationException;
use Doctrine\ORM\EntityManagerInterface;
use Doctrine\Persistence\ManagerRegistry;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\PasswordHasher\Hasher\PasswordHasherFactoryInterface;
use Symfony\Component\RateLimiter\RateLimiterFactoryInterface;

/**
 * Signing up with an email address, a name and a password. The account exists at once, with its personal
 * organization, but cannot sign in until the address is confirmed through the link mailed to it.
 *
 * The name and the password of a sign-up travel with its confirmation link ({@see \App\Entity\AccountLink}), and
 * become the account's when that link is confirmed ({@see EmailConfirmation}). A sign-up never changes an account
 * that exists already, confirmed or not:
 * - a new address gets an account and the confirmation link;
 * - an address whose account is still unconfirmed gets a new link for this sign-up, which ends the earlier link. The
 *   account itself stays as it is, so someone who signs up with another person's address changes nothing the owner
 *   of the mailbox could confirm by mistake: every link confirms only its own sign-up, and asks for its password;
 * - an address with a confirmed account gets a note that it has one, with the way to reset the password.
 *
 * What the page says afterwards never depends on which of the three it was, and neither does the time it takes:
 * every sign-up hashes its password, counts against the same limits, and sends its mail after the response. When a
 * limit refuses the mail ({@see AccountMailBudget}), nothing is stored and no link ends.
 *
 * At most 10 sign-ups an hour come from one network address.
 */
final readonly class Registration
{
    public function __construct(
        private UserRepository $users,
        private UserProvisioner $provisioner,
        private PasswordHasherFactoryInterface $passwordHashers,
        private AccountLinks $links,
        private AccountMail $mail,
        private AccountMailBudget $budget,
        private EntityManagerInterface $entityManager,
        private ManagerRegistry $registry,
        #[Autowire(service: 'limiter.registration')]
        private RateLimiterFactoryInterface $registrations,
    ) {
    }

    /**
     * @throws TooManyAttempts when this network address signed up too often
     */
    public function register(string $email, string $displayName, #[\SensitiveParameter] string $password, ?string $ip): void
    {
        $limit = $this->registrations->create(ClientAddress::key($ip))->consume();
        if (!$limit->isAccepted()) {
            throw new TooManyAttempts($limit->getRetryAfter());
        }

        // Before the address is looked up: one hash and one request counted, whatever the address turns out to be.
        $passwordHash = $this->passwordHashers->getPasswordHasher(User::class)->hash($password);
        $mayAsk = $this->budget->requesterMayAsk(AccountMailBudget::CONFIRMATION, $ip);

        $user = $this->users->findOneByEmail($email);
        if (!$mayAsk) {
            return;
        }
        if (null !== $user && $user->isEmailConfirmed()) {
            if ($this->budget->mayMail(AccountMailBudget::ALREADY_REGISTERED, $user->getEmail(), $ip)) {
                $this->mail->alreadyRegistered($user);
            }

            return;
        }
        if (!$this->budget->mayMail(AccountMailBudget::CONFIRMATION, $email, $ip)) {
            return;
        }

        try {
            [$user, $secret] = $this->signUp($user, $email, $displayName, $passwordHash);
        } catch (UniqueConstraintViolationException $exception) {
            // Another sign-up with this address made the account in the same moment. The failed write closed the
            // entity manager; with a new one this sign-up continues as one for an address that has an account.
            $this->registry->resetManager();
            $user = $this->users->findOneByEmail($email) ?? throw $exception;
            if ($user->isEmailConfirmed()) {
                return;
            }
            [$user, $secret] = $this->signUp($user, $email, $displayName, $passwordHash);
        }
        $this->mail->confirmation($user, $secret);
    }

    /**
     * A new confirmation link for an unconfirmed account (the sign-in page offers it), for the same sign-up as its
     * latest link; nothing for any other address.
     */
    public function resendConfirmation(string $email, ?string $ip): void
    {
        $mayAsk = $this->budget->requesterMayAsk(AccountMailBudget::CONFIRMATION, $ip);

        $user = $this->users->findOneByEmail($email);
        if (!$mayAsk || null === $user) {
            return;
        }
        $this->resend($user, fn (): bool => $this->budget->mayMail(AccountMailBudget::CONFIRMATION, $user->getEmail(), $ip));
    }

    /**
     * The same for someone who holds a confirmation link that expired: they were sent it, so what others asked for
     * this address does not count against them.
     */
    public function resendConfirmationWithLink(#[\SensitiveParameter] string $secret): void
    {
        $link = $this->links->known($secret, AccountLinkPurpose::ConfirmEmail);
        if (null === $link) {
            return;
        }
        $user = $link->getUser();
        $this->resend($user, fn (): bool => $this->budget->ownerMayMail(AccountMailBudget::CONFIRMATION, $user));
    }

    /**
     * The account of the sign-up (made now for a new address) and the secret of its confirmation link, stored
     * together or not at all.
     *
     * @return array{User, string}
     */
    private function signUp(?User $user, string $email, string $displayName, #[\SensitiveParameter] string $passwordHash): array
    {
        return $this->entityManager->wrapInTransaction(function () use ($user, $email, $displayName, $passwordHash): array {
            $user ??= $this->provisioner->provisionHashed($email, $displayName, $passwordHash, false);

            return [$user, $this->links->issue($user, AccountLinkPurpose::ConfirmEmail, $displayName, $passwordHash)];
        });
    }

    /** @param \Closure(): bool $mayMail asked last, so a request that sends nothing spends no mail */
    private function resend(User $user, \Closure $mayMail): void
    {
        if ($user->isEmailConfirmed()) {
            return;
        }
        $latest = $this->links->newest($user, AccountLinkPurpose::ConfirmEmail);
        if (null === $latest || null === $latest->getPendingPassword() || !$mayMail()) {
            return;
        }
        $this->mail->confirmation($user, $this->links->issue($user, AccountLinkPurpose::ConfirmEmail, $latest->getPendingDisplayName(), $latest->getPendingPassword()));
    }
}
