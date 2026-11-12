<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Entity\User;
use App\Service\UserProvisioner;
use Symfony\Bundle\FrameworkBundle\Console\Application;
use Symfony\Bundle\FrameworkBundle\KernelBrowser;
use Symfony\Component\Console\Tester\CommandTester;

/**
 * Obtains real tokens through the OAuth2 server, as the desktop app does: an authorization request with PKCE to a
 * loopback port, the browser sign-in it leads to, and a token request that names the device.
 *
 * @property KernelBrowser $client
 */
trait AuthenticatesUsers
{
    private const string DEFAULT_PASSWORD = 'correct-horse-battery-staple';
    private const string DESKTOP_REDIRECT_URI = 'http://127.0.0.1:53682/auth/callback';

    protected function bootstrapOAuthClients(): void
    {
        $tester = new CommandTester(new Application($this->client->getKernel())->find('app:oauth:bootstrap-clients'));
        $tester->execute([]);
        $tester->assertCommandIsSuccessful();
    }

    /** A user whose address is confirmed, so they can sign in. */
    protected function createUser(string $email, string $name = 'Test User'): User
    {
        $user = static::getContainer()->get(UserProvisioner::class)->provision($email, $name, self::DEFAULT_PASSWORD, true);
        $this->resetRateLimits();

        return $user;
    }

    /**
     * Signs the user in on a new desktop device and returns the request headers for API calls.
     *
     * @return array<string, string>
     */
    protected function userHeaders(string $email, string $deviceName = 'MacBook Pro', string $password = self::DEFAULT_PASSWORD): array
    {
        return self::bearer($this->signInDevice($email, $deviceName, $password)['access_token']);
    }

    /**
     * Signs the user in on a new desktop device.
     *
     * @return array{access_token: string, refresh_token: string}
     */
    protected function signInDevice(string $email, string $deviceName = 'MacBook Pro', string $password = self::DEFAULT_PASSWORD): array
    {
        $verifier = bin2hex(random_bytes(32));

        $this->client->getCookieJar()->clear();
        $this->client->request('GET', $this->desktopAuthorizeUrl($verifier));
        $crawler = $this->client->followRedirect();
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => $email, '_password' => $password]));
        if (str_ends_with((string) $this->client->getResponse()->headers->get('Location'), '/login')) {
            self::fail('The sign-in was refused: '.$this->client->followRedirect()->filter('.error')->text(''));
        }

        // Signed in for this very request: it goes on without asking.
        $this->client->followRedirect();
        $code = $this->codeFrom((string) $this->client->getResponse()->headers->get('Location'));

        $this->client->request('POST', '/oauth/token', $this->codeExchange($code, $verifier, $deviceName));
        self::assertResponseIsSuccessful();
        $this->client->getCookieJar()->clear();
        $tokens = $this->decodedResponse();

        return ['access_token' => self::string($tokens['access_token'] ?? null), 'refresh_token' => self::string($tokens['refresh_token'] ?? null)];
    }

    /**
     * An authorization request of the desktop app.
     *
     * @param array<string, string> $extra
     */
    protected function desktopAuthorizeUrl(string $verifier, array $extra = []): string
    {
        return '/oauth/authorize?'.http_build_query([
            'response_type' => 'code',
            'client_id' => 'desktop',
            'redirect_uri' => self::DESKTOP_REDIRECT_URI,
            'scope' => 'account sync',
            'state' => 's',
            'code_challenge' => self::challenge($verifier),
            'code_challenge_method' => 'S256',
            ...$extra,
        ]);
    }

    /**
     * Where an authorization request of the desktop app sends a browser that is signed in already: the page that asks
     * whether to sign in on this computer is answered with Continue. Returns the Location of the redirect that
     * follows (the callback with a code or an error) or of the redirect the request got at once (the sign-in page).
     */
    protected function authorizedCallback(string $authorizeUrl): string
    {
        $crawler = $this->client->request('GET', $authorizeUrl);
        if (200 === $this->client->getResponse()->getStatusCode()) {
            self::assertSelectorTextContains('h1', 'Sign in to Varve on this computer?');
            $this->client->submit($crawler->selectButton('Continue')->form());
        }

        return (string) $this->client->getResponse()->headers->get('Location');
    }

    /**
     * The token request of the desktop app for a code.
     *
     * @return array<string, string>
     */
    protected function codeExchange(string $code, string $verifier, string $deviceName = 'MacBook Pro'): array
    {
        return [
            'grant_type' => 'authorization_code',
            'client_id' => 'desktop',
            'redirect_uri' => self::DESKTOP_REDIRECT_URI,
            'code' => $code,
            'code_verifier' => $verifier,
            'device_name' => $deviceName,
            'device_platform' => 'macos',
            'app_version' => '0.4.0',
        ];
    }

    protected function codeFrom(string $callback): string
    {
        parse_str((string) parse_url($callback, \PHP_URL_QUERY), $query);
        $code = $query['code'] ?? null;
        self::assertIsString($code, 'Authorization did not return a code: '.$callback);

        return $code;
    }

    /** @param array<string, string> $extra */
    protected function refresh(string $refreshToken, array $extra = []): void
    {
        $this->client->request('POST', '/oauth/token', ['grant_type' => 'refresh_token', 'client_id' => 'desktop', 'refresh_token' => $refreshToken, ...$extra]);
    }

    protected function me(string $accessToken): void
    {
        $this->client->request('GET', '/api/v1/me', server: self::bearer($accessToken));
    }

    /** @return array<string, string> */
    protected static function bearer(string $accessToken): array
    {
        return ['HTTP_AUTHORIZATION' => 'Bearer '.$accessToken, 'HTTP_ACCEPT' => 'application/json'];
    }

    protected static function challenge(string $verifier): string
    {
        return rtrim(strtr(base64_encode(hash('sha256', $verifier, true)), '+/', '-_'), '=');
    }

    protected static function string(mixed $value): string
    {
        self::assertIsString($value);

        return $value;
    }
}
