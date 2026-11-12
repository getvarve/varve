<?php

declare(strict_types=1);

namespace App\Tests\Functional;

/**
 * What the authorization server says about itself, before anyone signs in.
 */
final class OAuthMetadataTest extends DatabaseTestCase
{
    public function testTheAuthorizationServerDescribesItselfBeforeSignIn(): void
    {
        $this->client->request('GET', '/.well-known/oauth-authorization-server');

        self::assertResponseIsSuccessful();
        $api = $this->apiAddress();
        self::assertSame([
            'issuer' => $api,
            'authorization_endpoint' => $api.'/oauth/authorize',
            'token_endpoint' => $api.'/oauth/token',
            'revocation_endpoint' => $api.'/oauth/revoke',
            'revocation_endpoint_auth_methods_supported' => ['none'],
            'scopes_supported' => ['account', 'sync'],
            'response_types_supported' => ['code'],
            'response_modes_supported' => ['query'],
            'prompt_values_supported' => ['login'],
            'grant_types_supported' => ['authorization_code', 'refresh_token'],
            'code_challenge_methods_supported' => ['S256'],
            'token_endpoint_auth_methods_supported' => ['none'],
        ], $this->decodedResponse());
        self::assertArrayNotHasKey('registration_endpoint', $this->decodedResponse(), 'No dynamic client registration.');
    }

    public function testTheApiNamesThisServerAsItsAuthorizationServer(): void
    {
        $this->client->request('GET', '/.well-known/oauth-protected-resource');

        self::assertResponseIsSuccessful();
        self::assertSame($this->apiAddress(), $this->decodedResponse()['resource'] ?? null);
        self::assertSame([$this->apiAddress()], $this->decodedResponse()['authorization_servers'] ?? null);
    }

    public function testThereIsNoClientRegistrationOrClientCredentials(): void
    {
        $this->client->request('POST', '/oauth/register', server: ['CONTENT_TYPE' => 'application/json'], content: '{"client_name":"x","redirect_uris":["https://x.example/cb"]}');
        self::assertResponseStatusCodeSame(404);

        $this->client->request('POST', '/oauth/token', ['grant_type' => 'client_credentials', 'client_id' => 'desktop']);
        self::assertResponseStatusCodeSame(400);
        self::assertSame('unsupported_grant_type', $this->decodedResponse()['error'] ?? null);
    }
}
