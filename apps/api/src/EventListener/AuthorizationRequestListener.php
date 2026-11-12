<?php

declare(strict_types=1);

namespace App\EventListener;

use App\Command\BootstrapOAuthClientsCommand;
use App\Security\FirstPartyClients;
use App\Security\OAuth\SignInConfirmation;
use League\Bundle\OAuth2ServerBundle\Event\AuthorizationRequestResolveEvent;
use League\Bundle\OAuth2ServerBundle\OAuth2Events;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;

/**
 * Answers authorization requests. Our own apps are the only clients this server has; any other client is denied
 * (`access_denied`), since there is no consent screen to ask the user with.
 *
 * The web client is approved at once: its code only ever goes to its one registered address. The desktop app returns
 * to this computer, where any program could be the one that asks, so the person at the browser is asked first unless
 * they just signed in for that request ({@see SignInConfirmation}).
 */
#[AsEventListener(event: OAuth2Events::AUTHORIZATION_REQUEST_RESOLVE)]
final readonly class AuthorizationRequestListener
{
    public function __construct(private SignInConfirmation $confirmation)
    {
    }

    public function __invoke(AuthorizationRequestResolveEvent $event): void
    {
        $client = $event->getClient()->getIdentifier();
        if (!FirstPartyClients::isFirstParty($client)) {
            $event->resolveAuthorization(AuthorizationRequestResolveEvent::AUTHORIZATION_DENIED);

            return;
        }
        if (BootstrapOAuthClientsCommand::DESKTOP === $client) {
            $this->confirmation->resolve($event);

            return;
        }
        $event->resolveAuthorization(AuthorizationRequestResolveEvent::AUTHORIZATION_APPROVED);
    }
}
