<?php

declare(strict_types=1);

namespace App\EventListener;

use App\Security\FirstPartyClients;
use League\Bundle\OAuth2ServerBundle\Event\AuthorizationRequestResolveEvent;
use League\Bundle\OAuth2ServerBundle\OAuth2Events;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;

/**
 * Approves authorization requests from our own apps without a consent screen. They are the only clients this server
 * has; any other client is denied (`access_denied`), since there is no consent screen to ask the user with.
 */
#[AsEventListener(event: OAuth2Events::AUTHORIZATION_REQUEST_RESOLVE)]
final readonly class AuthorizationRequestListener
{
    public function __invoke(AuthorizationRequestResolveEvent $event): void
    {
        $event->resolveAuthorization(FirstPartyClients::isFirstParty($event->getClient()->getIdentifier())
            ? AuthorizationRequestResolveEvent::AUTHORIZATION_APPROVED
            : AuthorizationRequestResolveEvent::AUTHORIZATION_DENIED);
    }
}
