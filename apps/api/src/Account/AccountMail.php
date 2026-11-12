<?php

declare(strict_types=1);

namespace App\Account;

use App\Deployment\PublicAddresses;
use App\Entity\User;
use Symfony\Bridge\Twig\Mime\TemplatedEmail;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\Mime\Address;
use Symfony\Component\Routing\Generator\UrlGeneratorInterface;

/**
 * The mail the account pages send: the confirmation link of a new account, a password reset link, and the note to an
 * address that signed up again although it has an account. Links are built on API_PUBLIC_URL, never on the host of
 * the request that asked.
 *
 * A mail leaves after the response of the request that asked for it ({@see AccountMailDelivery}), and how many are
 * sent is limited ({@see AccountMailBudget}).
 *
 * A mail to an address nobody has confirmed carries nothing the person who signed up chose: the confirmation mail
 * names nobody, and a password reset greets by name only once the account is confirmed. Otherwise anyone could have
 * this server send a sentence of theirs to someone else's mailbox.
 */
final readonly class AccountMail
{
    public function __construct(
        private AccountMailDelivery $delivery,
        private UrlGeneratorInterface $urls,
        private PublicAddresses $addresses,
        #[Autowire(env: 'VARVE_MAIL_FROM')]
        private string $from,
    ) {
    }

    public function confirmation(User $user, #[\SensitiveParameter] string $secret): void
    {
        $this->delivery->send($this->email($user->getEmail(), 'Confirm your email address for Varve', 'confirm_email', [
            'link' => $this->link('app_confirm_email', $secret),
        ]));
    }

    public function passwordReset(User $user, #[\SensitiveParameter] string $secret): void
    {
        $this->delivery->send($this->email($user->getEmail(), 'Reset your Varve password', 'reset_password', [
            'name' => $user->isEmailConfirmed() ? $user->getDisplayName() : null,
            'link' => $this->link('app_reset_password', $secret),
        ]));
    }

    public function alreadyRegistered(User $user): void
    {
        $this->delivery->send($this->email($user->getEmail(), 'You already have a Varve account', 'already_registered', [
            'name' => $user->getDisplayName(),
            'sign_in' => $this->addresses->api().$this->urls->generate('app_login'),
            'reset' => $this->addresses->api().$this->urls->generate('app_forgot_password'),
        ]));
    }

    /** @param array<string, string|null> $context */
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
