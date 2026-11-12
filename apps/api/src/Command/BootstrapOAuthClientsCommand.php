<?php

declare(strict_types=1);

namespace App\Command;

use App\Deployment\PublicAddresses;
use App\Security\Scope;
use League\Bundle\OAuth2ServerBundle\Manager\ClientManagerInterface;
use League\Bundle\OAuth2ServerBundle\Model\AbstractClient;
use League\Bundle\OAuth2ServerBundle\Model\Client;
use League\Bundle\OAuth2ServerBundle\OAuth2Grants;
use League\Bundle\OAuth2ServerBundle\ValueObject\Grant;
use League\Bundle\OAuth2ServerBundle\ValueObject\RedirectUri;
use League\Bundle\OAuth2ServerBundle\ValueObject\Scope as OAuthScope;
use Symfony\Component\Console\Attribute\AsCommand;
use Symfony\Component\Console\Command\Command;
use Symfony\Component\Console\Style\SymfonyStyle;

/**
 * Creates or updates the first-party OAuth2 clients. Safe to run on every deploy.
 *
 * Both are public clients (no secret) that must use PKCE with S256:
 * - `desktop`: the desktop app returns either through its own scheme or to a port it listens on for the moment of the
 *   sign-in on this computer (RFC 8252 7.3: any port of 127.0.0.1 or [::1] matches the registered loopback URI);
 * - `web`: the web client returns to <WEB_APP_URL>/auth/callback. While WEB_APP_URL is empty this server has no web
 *   client, and the client is kept but inactive, so nothing can sign in as it.
 */
#[AsCommand(name: 'app:oauth:bootstrap-clients', description: 'Create or update the first-party OAuth2 clients')]
final readonly class BootstrapOAuthClientsCommand
{
    public const string DESKTOP = 'desktop';
    public const string WEB = 'web';

    /** The desktop app's own scheme. */
    public const string DESKTOP_SCHEME_REDIRECT_URI = 'varve://auth/callback';

    /** @var list<non-empty-string> */
    public const array DESKTOP_LOOPBACK_REDIRECT_URIS = ['http://127.0.0.1/auth/callback', 'http://[::1]/auth/callback'];

    public function __construct(
        private ClientManagerInterface $clients,
        private PublicAddresses $addresses,
    ) {
    }

    public function __invoke(SymfonyStyle $io): int
    {
        $scopes = Scope::values();
        $grants = [OAuth2Grants::AUTHORIZATION_CODE, OAuth2Grants::REFRESH_TOKEN];

        $this->upsert(self::DESKTOP, 'Varve desktop app', true, $grants, [self::DESKTOP_SCHEME_REDIRECT_URI, ...self::DESKTOP_LOOPBACK_REDIRECT_URIS], $scopes);

        $web = $this->addresses->web();
        $this->upsert(self::WEB, 'Varve web client', null !== $web, $grants, null === $web ? [] : [$web.'/auth/callback'], $scopes);

        $io->success(null === $web
            ? 'First-party OAuth2 clients are up to date. The web client is off: WEB_APP_URL is empty.'
            : 'First-party OAuth2 clients are up to date.');

        return Command::SUCCESS;
    }

    /**
     * @param non-empty-string       $identifier
     * @param list<non-empty-string> $grants
     * @param list<non-empty-string> $redirectUris
     * @param list<string>           $scopes
     */
    private function upsert(string $identifier, string $name, bool $active, array $grants, array $redirectUris, array $scopes): void
    {
        $client = $this->clients->find($identifier);
        if (null === $client) {
            $client = new Client($name, $identifier, null);
        } elseif (!$client instanceof AbstractClient) {
            throw new \LogicException(\sprintf('OAuth2 client "%s" is not a %s.', $identifier, AbstractClient::class));
        } else {
            $client->setName($name);
        }

        $client->setActive($active);
        $client->setAllowPlainTextPkce(false);
        $client->setGrants(...array_map(static fn (string $grant): Grant => new Grant($grant), $grants));
        $client->setRedirectUris(...array_map(static fn (string $uri): RedirectUri => new RedirectUri($uri), $redirectUris));
        $client->setScopes(...array_map(static fn (string $scope): OAuthScope => new OAuthScope($scope), array_values(array_filter($scopes, static fn (string $scope): bool => '' !== $scope))));

        $this->clients->save($client);
    }
}
