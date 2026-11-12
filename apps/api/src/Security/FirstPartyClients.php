<?php

declare(strict_types=1);

namespace App\Security;

use App\Command\BootstrapOAuthClientsCommand;
use League\Bundle\OAuth2ServerBundle\Security\Authentication\Token\OAuth2Token;
use Symfony\Component\Security\Core\Authentication\Token\TokenInterface;

/**
 * Our own apps: the desktop app and the web client. They are approved without a consent screen, and only they may do
 * what a user should only do in Varve itself, such as changing the password, deleting the account or signing out
 * devices. Today they are the only clients there are; this keeps those operations theirs when others come.
 */
final class FirstPartyClients
{
    /** Security attribute: the request comes from one of our own apps, on behalf of a user. */
    public const string ATTRIBUTE = 'FIRST_PARTY_CLIENT';

    public const array IDENTIFIERS = [
        BootstrapOAuthClientsCommand::DESKTOP,
        BootstrapOAuthClientsCommand::WEB,
    ];

    public static function isFirstParty(string $clientIdentifier): bool
    {
        return \in_array($clientIdentifier, self::IDENTIFIERS, true);
    }

    /** Whether the request was signed with an OAuth2 access token issued to one of our own apps. */
    public static function signedRequest(?TokenInterface $token): bool
    {
        return $token instanceof OAuth2Token && self::isFirstParty($token->getOAuthClientId());
    }
}
