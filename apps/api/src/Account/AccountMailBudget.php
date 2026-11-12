<?php

declare(strict_types=1);

namespace App\Account;

use App\Entity\User;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\RateLimiter\RateLimiterFactoryInterface;

/**
 * How much account mail the pages send, so they cannot be used to flood a mailbox, and so one person cannot use up
 * the mail another needs. Every budget is counted per kind of mail: asking for confirmation links spends nothing of
 * an address's password reset links.
 *
 * Someone who only names an address ({@see self::requesterMayAsk()}, {@see self::mayMail()}):
 * - 20 requests an hour per network address that asks, counted for every request, whatever the address and whether or
 *   not it has an account;
 * - 5 mails a day to one address asked for from one network address;
 * - 30 mails a day to one address from all network addresses together.
 *
 * Someone who has shown the mailbox or the account is theirs, with a link sent to it earlier or a signed-in browser
 * ({@see self::ownerMayMail()}): 10 mails a day per account, counted apart from all of the above, so nothing a
 * stranger asks for keeps the owner from a new link.
 *
 * A refused mail is not sent and the page says the same as always.
 */
final readonly class AccountMailBudget
{
    /** A confirmation link: a sign-up, or asking for the link again. */
    public const string CONFIRMATION = 'confirmation';
    /** A password reset link. */
    public const string PASSWORD_RESET = 'password_reset';
    /** The note to an address that signed up although it has an account. */
    public const string ALREADY_REGISTERED = 'already_registered';

    public function __construct(
        #[Autowire(service: 'limiter.account_mail_requester')]
        private RateLimiterFactoryInterface $perRequester,
        #[Autowire(service: 'limiter.account_mail_recipient_requester')]
        private RateLimiterFactoryInterface $perRecipientAndRequester,
        #[Autowire(service: 'limiter.account_mail_recipient')]
        private RateLimiterFactoryInterface $perRecipient,
        #[Autowire(service: 'limiter.account_mail_owner')]
        private RateLimiterFactoryInterface $perOwner,
    ) {
    }

    /** Counts one request of this network address; taken before anything is looked up. */
    public function requesterMayAsk(string $kind, ?string $ip): bool
    {
        return $this->perRequester->create($kind.'|'.ClientAddress::key($ip))->consume()->isAccepted();
    }

    /**
     * Takes one mail to this address, asked for from this network address. A refusal for the pair spends nothing of
     * what the address may get from everyone together.
     */
    public function mayMail(string $kind, string $address, ?string $ip): bool
    {
        $recipient = $kind.'|'.hash('sha256', mb_strtolower($address));

        return $this->perRecipientAndRequester->create($recipient.'|'.ClientAddress::key($ip))->consume()->isAccepted()
            && $this->perRecipient->create($recipient)->consume()->isAccepted();
    }

    /** Takes one mail the owner of the account asked for. */
    public function ownerMayMail(string $kind, User $user): bool
    {
        return $this->perOwner->create($kind.'|'.$user->getId()->toRfc4122())->consume()->isAccepted();
    }
}
