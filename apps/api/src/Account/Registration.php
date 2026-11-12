<?php

declare(strict_types=1);

namespace App\Account;

use App\Entity\User;
use App\Enum\AccountLinkPurpose;
use App\Repository\UserRepository;
use App\Service\UserProvisioner;
use Doctrine\ORM\EntityManagerInterface;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\Mailer\Exception\TransportExceptionInterface;
use Symfony\Component\PasswordHasher\Hasher\UserPasswordHasherInterface;
use Symfony\Component\RateLimiter\RateLimiterFactoryInterface;

/**
 * Signing up with an email address, a name and a password. The account exists at once, with its personal
 * organization, but cannot sign in until the address is confirmed through the link mailed to it.
 *
 * What the page says afterwards never depends on whether the address had an account, so the form cannot be used to
 * find out who uses Varve:
 * - a new address gets the confirmation link;
 * - an address whose account is still unconfirmed gets a new link, and the name and password of this sign-up replace
 *   the earlier ones: someone who signed up with another person's address cannot keep the account that way, since
 *   only the owner of the mailbox can confirm it, and only with the latest link;
 * - an address with a confirmed account gets a note that it has one, with the way to reset the password.
 *
 * At most 10 sign-ups an hour come from one network address.
 */
final readonly class Registration
{
    public function __construct(
        private UserRepository $users,
        private UserProvisioner $provisioner,
        private UserPasswordHasherInterface $passwordHasher,
        private AccountLinks $links,
        private AccountMail $mail,
        private EntityManagerInterface $entityManager,
        #[Autowire(service: 'limiter.registration')]
        private RateLimiterFactoryInterface $registrations,
    ) {
    }

    /**
     * @throws TooManyAttempts             when this network address signed up too often
     * @throws TransportExceptionInterface when the mail server refuses the mail
     */
    public function register(string $email, string $displayName, #[\SensitiveParameter] string $password, ?string $ip): void
    {
        $limit = $this->registrations->create(ClientAddress::key($ip))->consume();
        if (!$limit->isAccepted()) {
            throw new TooManyAttempts($limit->getRetryAfter());
        }

        $user = $this->users->findOneByEmail($email);
        if (null === $user) {
            $user = $this->provisioner->provision($email, $displayName, $password, false);
            $this->sendConfirmation($user, $ip);

            return;
        }

        if ($user->isEmailConfirmed()) {
            if ($this->mail->mayMail($user->getEmail(), $ip)) {
                $this->mail->alreadyRegistered($user);
            }

            return;
        }

        $user->setDisplayName($displayName);
        $user->setPassword($this->passwordHasher->hashPassword($user, $password));
        $this->entityManager->flush();
        $this->sendConfirmation($user, $ip);
    }

    /**
     * A new confirmation link for an unconfirmed account (the sign-in page offers it); nothing for any other address.
     *
     * @throws TransportExceptionInterface when the mail server refuses the mail
     */
    public function resendConfirmation(string $email, ?string $ip): void
    {
        $user = $this->users->findOneByEmail($email);
        if (null !== $user && !$user->isEmailConfirmed()) {
            $this->sendConfirmation($user, $ip);
        }
    }

    private function sendConfirmation(User $user, ?string $ip): void
    {
        if (!$this->mail->mayMail($user->getEmail(), $ip)) {
            return;
        }
        $this->mail->confirmation($user, $this->links->issue($user, AccountLinkPurpose::ConfirmEmail));
    }
}
