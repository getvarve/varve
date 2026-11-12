<?php

declare(strict_types=1);

namespace App\Account;

use Psr\Log\LoggerInterface;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;
use Symfony\Component\HttpFoundation\RequestStack;
use Symfony\Component\HttpKernel\KernelEvents;
use Symfony\Component\Mailer\MailerInterface;
use Symfony\Component\Mime\Email;
use Symfony\Contracts\Service\ResetInterface;

/**
 * Hands account mail to the mailer once the response of the request that asked for it is decided and sent, so neither
 * the time a mail takes nor a mail server that is down tells anyone whether an address has an account: every address
 * gets the same page, as fast.
 *
 * The mailer sends through the messenger (`mail` in config/packages/messenger.yaml): to a queue a worker empties where
 * the server has one, else right here, after the response. A mail that cannot be handed over or sent is logged, never
 * shown.
 *
 * Outside a request (a command) there is no response to wait for, and the mail goes at once.
 */
final class AccountMailDelivery implements ResetInterface
{
    /** @var list<Email> */
    private array $waiting = [];

    public function __construct(
        private readonly MailerInterface $mailer,
        private readonly RequestStack $requests,
        private readonly LoggerInterface $logger,
    ) {
    }

    public function send(Email $email): void
    {
        if (null === $this->requests->getMainRequest()) {
            $this->deliver($email);

            return;
        }
        $this->waiting[] = $email;
    }

    #[AsEventListener(event: KernelEvents::TERMINATE)]
    public function afterResponse(): void
    {
        $waiting = $this->waiting;
        $this->waiting = [];
        foreach ($waiting as $email) {
            $this->deliver($email);
        }
    }

    public function reset(): void
    {
        $this->waiting = [];
    }

    private function deliver(Email $email): void
    {
        try {
            $this->mailer->send($email);
        } catch (\Throwable $exception) {
            // Whatever went wrong (the mail server, the queue, the template): the response is out, and the next mail
            // must still go.
            $this->logger->error('Account mail could not be sent: {message}', ['message' => $exception->getMessage(), 'exception' => $exception]);
        }
    }
}
