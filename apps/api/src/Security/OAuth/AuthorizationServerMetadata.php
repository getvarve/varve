<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use App\Controller\OAuthRevocationController;
use App\Deployment\PublicAddresses;
use App\Security\Scope;
use Symfony\Component\Routing\Generator\UrlGeneratorInterface;

/**
 * The authorization server metadata (RFC 8414): where our apps sign in on this server and how. Everything is addressed
 * from API_PUBLIC_URL, which is also the issuer. Only public clients with PKCE (S256) and only the authorization code
 * and refresh token grants; there is no registration endpoint, since only our own apps sign in.
 *
 * There is no `/.well-known/openid-configuration`: this server does not issue ID tokens.
 */
final readonly class AuthorizationServerMetadata
{
    public const string PATH = '/.well-known/oauth-authorization-server';

    public function __construct(
        private PublicAddresses $addresses,
        private UrlGeneratorInterface $urls,
    ) {
    }

    /** @return array<string, mixed> */
    public function document(): array
    {
        $issuer = $this->addresses->api();

        return [
            'issuer' => $issuer,
            'authorization_endpoint' => $issuer.$this->urls->generate('oauth2_authorize'),
            'token_endpoint' => $issuer.$this->urls->generate('oauth2_token'),
            'revocation_endpoint' => $issuer.$this->urls->generate(OAuthRevocationController::ROUTE),
            'revocation_endpoint_auth_methods_supported' => ['none'],
            'scopes_supported' => Scope::values(),
            'response_types_supported' => ['code'],
            'response_modes_supported' => ['query'],
            // prompt=login asks for the password again whatever the browser's sign-in ({@see PromptLogin}).
            'prompt_values_supported' => [PromptLogin::LOGIN],
            'grant_types_supported' => ['authorization_code', 'refresh_token'],
            'code_challenge_methods_supported' => ['S256'],
            'token_endpoint_auth_methods_supported' => ['none'],
        ];
    }

    /**
     * The protected resource metadata (RFC 9728) of the API: this server is its authorization server.
     *
     * @return array<string, mixed>
     */
    public function apiResource(): array
    {
        return [
            'resource' => $this->addresses->api(),
            'resource_name' => 'Varve API',
            'authorization_servers' => [$this->addresses->api()],
            'scopes_supported' => Scope::values(),
            'bearer_methods_supported' => ['header'],
        ];
    }
}
