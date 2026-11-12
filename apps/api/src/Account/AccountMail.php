<?php

declare(strict_types=1);

namespace App\Account;

use App\Deployment\PublicAddresses;
use App\Entity\User;
use Symfony\Bridge\Twig\Mime\TemplatedEmail;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\Mailer\Exception\TransportExceptionInterface;
use Symfony\Component\Mailer\MailerInterface;
use Symfony\Component\Mime\Address;
use Symfony\Component\RateLimiter\RateLimiterFactoryInterface;
use Symfony\Component\Routing\Generator\UrlGeneratorInterface;

/**
 * The mail the account pages send: the confirmation link of a new account, a password reset link, and the note to an
 * address that signed up again although it has an account. Links are built on API_PUBLIC_URL, never on the host of
 * the request that asked.
 *
 * Not every request gets a mail ({@see self::mayMail()}): 5 a day per receiving address, whoever asks, and 20 an hour
 * per network address that asks, so the pages cannot be used to flood someone's mailbox. A refused mail is not sent
 * and the page says the same as always, so nobody learns from it whether an address has an account.
 */
final readonly class AccountMail
{
    public function __construct(
        private MailerInterface $mailer,
        private UrlGeneratorInterface $urls,
        private PublicAddresses $addresses,
        #[Autowire(env: 'VARVE_MAIL_FROM')]
        private string $from,
        #[Autowire(service: 'limiter.account_mail_recipient')]
        private RateLimiterFactoryInterface $perRecipient,
        #[Autowire(service: 'limiter.account_mail_sender')]
        private RateLimiterFactoryInterface $perSender,
    ) {
    }

    /**
     * Takes one mail from both limits: per network address that asks, then per receiving address. A refusal by the
     * first spends nothing of the second.
     */
    public function mayMail(string $address, ?string $ip): bool
    {
        return $this->perSender->create(ClientAddress::key($ip))->consume()->isAccepted()
            && $this->perRecipient->create(hash('sha256', mb_strtolower($address)))->consume()->isAccepted();
    }

    /** @throws TransportExceptionInterface when the mail server refuses it */
    public function confirmation(User $user, #[\SensitiveParameter] string $secret): void
    {
        $this->mailer->send($this->email($user->getEmail(), 'Confirm your email address for Varve', 'confirm_email', [
            'name' => $user->getDisplayName(),
            'link' => $this->link('app_confirm_email', $secret),
        ]));
    }

    /** @throws TransportExceptionInterface when the mail server refuses it */
    public function passwordReset(User $user, #[\SensitiveParameter] string $secret): void
    {
        $this->mailer->send($this->email($user->getEmail(), 'Reset your Varve password', 'reset_password', [
            'name' => $user->getDisplayName(),
            'link' => $this->link('app_reset_password', $secret),
        ]));
    }

    /** @throws TransportExceptionInterface when the mail server refuses it */
    public function alreadyRegistered(User $user): void
    {
        $this->mailer->send($this->email($user->getEmail(), 'You already have a Varve account', 'already_registered', [
            'name' => $user->getDisplayName(),
            'sign_in' => $this->addresses->api().$this->urls->generate('app_login'),
            'reset' => $this->addresses->api().$this->urls->generate('app_forgot_password'),
        ]));
    }

    /** @param array<string, string> $context */
    private function email(string $to, string $subject, string $template, array $context): TemplatedEmail
    {
        $email = new TemplatedEmail()
            ->from(Address::create($this->from))
            ->to($to)
            ->subject($subject)
            ->htmlTemplate('emails/'.$template.'.html.twig')
            ->textTemplate('emails/'.$template.'.txt.twig')
            ->context([...$context, 'app_address' => $this->addresses->api()]);
        $email->getHeaders()->addTextHeader('Auto-Submitted', 'auto-generated');

        return $email;
    }

    private function link(string $route, #[\SensitiveParameter] string $secret): string
    {
        return $this->addresses->api().$this->urls->generate($route, ['token' => $secret]);
    }
}
