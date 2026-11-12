<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Security\OAuth\RefreshTokenRotation;
use Doctrine\ORM\EntityManagerInterface;
use League\Bundle\OAuth2ServerBundle\Manager\ClientManagerInterface;
use League\Bundle\OAuth2ServerBundle\Model\Client;
use League\Bundle\OAuth2ServerBundle\OAuth2Grants;
use League\Bundle\OAuth2ServerBundle\ValueObject\Grant;
use League\Bundle\OAuth2ServerBundle\ValueObject\RedirectUri;
use Symfony\Component\Clock\Test\ClockSensitiveTrait;
use Symfony\Component\DomCrawler\Field\ChoiceFormField;

/**
 * The first-party sign-in as the desktop app performs it: browser sign-in, authorization code with PKCE, token
 * exchange, then authenticated API calls; and refreshing, revoking and signing out.
 */
final class SignInFlowTest extends DatabaseTestCase
{
    use AuthenticatesUsers;
    use ClockSensitiveTrait;

    private const string EMAIL = 'kim@example.test';
    private const string PASSWORD = self::DEFAULT_PASSWORD;

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->createUser(self::EMAIL, 'Kim Visser');
    }

    public function testTheApiRequiresAToken(): void
    {
        $this->client->request('GET', '/api/v1/me', server: ['HTTP_ACCEPT' => 'application/json']);
        self::assertResponseStatusCodeSame(401);

        $this->client->request('GET', '/api/v1/devices', server: ['HTTP_ACCEPT' => 'application/json']);
        self::assertResponseStatusCodeSame(401);

        $this->me('not-a-token');
        self::assertResponseStatusCodeSame(401);
    }

    public function testTheSignInPageCarriesVarvesLookAndItsFilesExist(): void
    {
        $crawler = $this->client->request('GET', '/login');

        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Sign in');
        self::assertSelectorTextContains('.brand', 'Varve');
        self::assertResponseHeaderSame('X-Frame-Options', 'DENY');
        $public = self::string(static::getContainer()->getParameter('kernel.project_dir')).'/public';
        foreach (['link[rel="stylesheet"]' => 'href', 'link[rel="icon"]' => 'href'] as $selector => $attribute) {
            self::assertFileExists($public.self::string($crawler->filter($selector)->attr($attribute)), $selector.' points at a file the server has.');
        }
    }

    public function testTheDesktopAppSignsInOnAnyLoopbackPortAndReadsMe(): void
    {
        $verifier = bin2hex(random_bytes(32));
        $authorize = $this->desktopAuthorizeUrl($verifier, ['state' => 'state-1', 'redirect_uri' => 'http://127.0.0.1:61234/auth/callback']);

        $this->client->request('GET', $authorize);
        self::assertResponseRedirects('http://localhost/login');

        $crawler = $this->client->request('GET', '/login');
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => self::EMAIL, '_password' => self::PASSWORD]));
        $this->client->followRedirect();

        $callback = (string) $this->client->getResponse()->headers->get('Location');
        self::assertStringStartsWith('http://127.0.0.1:61234/auth/callback?', $callback, 'Signed in for this very request: it goes on without a question.');
        parse_str((string) parse_url($callback, \PHP_URL_QUERY), $query);
        self::assertSame('state-1', $query['state'] ?? null);

        $this->client->request('POST', '/oauth/token', [...$this->codeExchange($this->codeFrom($callback), $verifier), 'redirect_uri' => 'http://127.0.0.1:61234/auth/callback']);
        self::assertResponseIsSuccessful();
        $tokens = $this->decodedResponse();
        self::assertIsString($tokens['refresh_token'] ?? null);

        // The browser session from the login must not authenticate API calls; only the bearer token does.
        $this->client->getCookieJar()->clear();
        $this->me(self::string($tokens['access_token'] ?? null));

        self::assertResponseIsSuccessful();
        $me = $this->decodedResponse();
        self::assertSame(self::EMAIL, $me['email'] ?? null);
        self::assertSame('Kim Visser', $me['displayName'] ?? null);
    }

    public function testTheDesktopAppMayReturnThroughItsOwnScheme(): void
    {
        $this->signIn();
        $verifier = bin2hex(random_bytes(32));

        $callback = $this->authorizedCallback($this->desktopAuthorizeUrl($verifier, ['redirect_uri' => 'varve://auth/callback']));

        self::assertStringStartsWith('varve://auth/callback?', $callback);
    }

    public function testARedirectUriTheAppDidNotRegisterIsRefused(): void
    {
        $this->signIn();

        foreach (['http://localhost:61234/auth/callback', 'http://127.0.0.1:61234/elsewhere', 'https://evil.example/auth/callback', 'varve://other'] as $uri) {
            $this->client->request('GET', $this->desktopAuthorizeUrl(bin2hex(random_bytes(32)), ['redirect_uri' => $uri]));
            self::assertResponseStatusCodeSame(401, $uri);
            self::assertSame('invalid_client', $this->decodedResponse()['error'] ?? null, $uri);
        }
    }

    public function testTheWebClientSignsInAtItsCallback(): void
    {
        $this->signIn();
        $verifier = bin2hex(random_bytes(32));
        $redirectUri = 'http://localhost:5173/auth/callback';

        $this->client->request('GET', '/oauth/authorize?'.http_build_query([
            'response_type' => 'code', 'client_id' => 'web', 'redirect_uri' => $redirectUri, 'scope' => 'account sync', 'state' => 's',
            'code_challenge' => self::challenge($verifier), 'code_challenge_method' => 'S256',
        ]));
        $code = $this->codeFrom((string) $this->client->getResponse()->headers->get('Location'));
        $this->client->request('POST', '/oauth/token', [
            'grant_type' => 'authorization_code', 'client_id' => 'web', 'redirect_uri' => $redirectUri, 'code' => $code, 'code_verifier' => $verifier,
            'device_name' => 'Firefox on Linux', 'device_platform' => 'web', 'app_version' => '0.4.0',
        ]);

        self::assertResponseIsSuccessful();
    }

    public function testAClientThatIsNotOursCannotSignIn(): void
    {
        $this->signIn();

        $this->client->request('GET', $this->desktopAuthorizeUrl(bin2hex(random_bytes(32)), ['client_id' => 'someone-else']));

        self::assertResponseStatusCodeSame(401);
        self::assertSame('invalid_client', $this->decodedResponse()['error'] ?? null);
    }

    public function testPkceVerifierMustMatch(): void
    {
        $this->signIn();
        $code = $this->codeFrom($this->authorizedCallback($this->desktopAuthorizeUrl('the-real-verifier-'.str_repeat('x', 30))));

        $this->client->request('POST', '/oauth/token', $this->codeExchange($code, 'a-different-verifier-'.str_repeat('y', 30)));

        self::assertResponseStatusCodeSame(400);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM device'), 'No device without tokens.');
    }

    public function testAPublicClientMustUsePkce(): void
    {
        $this->signIn();
        $url = $this->desktopAuthorizeUrl(bin2hex(random_bytes(32)));
        parse_str((string) parse_url($url, \PHP_URL_QUERY), $query);
        unset($query['code_challenge'], $query['code_challenge_method']);

        $this->client->request('GET', '/oauth/authorize?'.http_build_query($query));

        self::assertResponseStatusCodeSame(400);
        self::assertFalse($this->client->getResponse()->isRedirection(), 'No code goes anywhere.');
        self::assertSame('invalid_request', $this->decodedResponse()['error'] ?? null);
    }

    public function testPlainPkceIsRefused(): void
    {
        $this->signIn();
        $verifier = bin2hex(random_bytes(32));

        $this->client->request('GET', $this->desktopAuthorizeUrl($verifier, ['code_challenge' => $verifier, 'code_challenge_method' => 'plain']));

        self::assertResponseStatusCodeSame(400);
        self::assertSame('invalid_request', $this->decodedResponse()['error'] ?? null);
    }

    public function testAnAuthorizationCodeWorksOnce(): void
    {
        $this->signIn();
        $verifier = bin2hex(random_bytes(32));
        $code = $this->codeFrom($this->authorizedCallback($this->desktopAuthorizeUrl($verifier)));
        $this->client->request('POST', '/oauth/token', $this->codeExchange($code, $verifier));
        self::assertResponseIsSuccessful();

        $this->client->request('POST', '/oauth/token', $this->codeExchange($code, $verifier));

        self::assertResponseStatusCodeSame(400);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
    }

    public function testWrongPasswordsLockTheFormForAWhile(): void
    {
        for ($attempt = 1; $attempt <= 5; ++$attempt) {
            $this->signIn(password: 'wrong-'.$attempt);
            $this->client->followRedirect();
            self::assertSelectorTextContains('.error', 'Invalid credentials', 'Attempt '.$attempt);
        }

        $this->signIn();
        $this->client->followRedirect();

        self::assertSelectorTextContains('.error', 'Too many failed login attempts');
        self::assertSame('http://localhost/login', $this->authorize(), 'Even the right password is refused for now.');
    }

    public function testAnUnconfirmedAddressCannotSignIn(): void
    {
        $this->connection()->executeStatement('UPDATE app_user SET email_confirmed_at = NULL WHERE email = :email', ['email' => self::EMAIL]);
        static::getContainer()->get(EntityManagerInterface::class)->clear();

        $this->signIn(password: 'wrong-password');
        $this->client->followRedirect();
        self::assertSelectorTextContains('.error', 'Invalid credentials', 'A wrong password says nothing about the account.');

        $this->signIn();
        $this->client->followRedirect();
        self::assertSelectorTextContains('.error', 'Confirm your email address first');
        self::assertSelectorExists('.error a[href^="/confirm-email/resend"]');
        self::assertSame('http://localhost/login', $this->authorize());
    }

    public function testASignInOpenedDirectlyEndsOnAPageThatSaysSo(): void
    {
        $this->signIn();

        self::assertResponseRedirects('http://localhost/signed-in');
        $this->client->followRedirect();
        self::assertSelectorTextContains('h1', 'You are signed in');
        self::assertSelectorTextContains('.lead', self::EMAIL);
    }

    public function testKeepMeSignedInSignsTheBrowserInAgainAfterTheSession(): void
    {
        $this->signIn();
        self::assertNotNull($this->client->getCookieJar()->get('REMEMBERME'), 'Keep me signed in is on by default.');

        $this->dropSession();

        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?', $this->authorize(), 'The remember-me cookie signs the browser in again.');
    }

    public function testWithoutKeepMeSignedInTheSignInEndsWithTheSession(): void
    {
        $this->signIn(rememberMe: false);
        self::assertNull($this->client->getCookieJar()->get('REMEMBERME'));
        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?', $this->authorize());

        $this->dropSession();

        self::assertSame('http://localhost/login', $this->authorize());
    }

    public function testAPasswordChangeEndsTheRememberedSignIn(): void
    {
        $this->signIn();
        $this->connection()->executeStatement("UPDATE app_user SET password = 'another-hash' WHERE email = :email", ['email' => self::EMAIL]);
        static::getContainer()->get(EntityManagerInterface::class)->clear();
        $this->dropSession();

        self::assertSame('http://localhost/login', $this->authorize());
    }

    public function testTheSignOutLinkOnlyShowsAPage(): void
    {
        $this->signIn();

        $crawler = $this->client->request('GET', '/logout');

        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Sign out');
        self::assertResponseHeaderSame('X-Frame-Options', 'DENY');
        self::assertResponseHeaderSame('Referrer-Policy', 'same-origin');
        self::assertSame(1, $crawler->filter('form[method="post"][action="/logout"]')->count());
        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?', $this->authorize(), 'Still signed in.');
    }

    public function testTheSignOutPageSignsOutAndGoesToTheWebClient(): void
    {
        $this->signIn();

        $crawler = $this->client->request('GET', '/logout');
        $this->client->submit($crawler->selectButton('Sign out')->form());

        self::assertResponseRedirects('http://localhost:5173');
        self::assertNull($this->client->getCookieJar()->get('REMEMBERME'), 'The remember-me cookie is cleared.');
        self::assertSame('http://localhost/login', $this->authorize());
    }

    public function testTheWebClientSignsOutWithAFormPost(): void
    {
        $this->signIn();

        $this->client->request('POST', '/logout', server: ['HTTP_ORIGIN' => 'http://localhost:5173']);

        self::assertResponseRedirects('http://localhost:5173');
        self::assertSame('http://localhost/login', $this->authorize());
    }

    public function testAnotherSiteCannotSignPeopleOut(): void
    {
        $this->signIn();

        $this->client->request('POST', '/logout', server: ['HTTP_ORIGIN' => 'https://evil.example']);
        self::assertResponseStatusCodeSame(403);
        $this->client->request('POST', '/logout', ['_csrf_token' => 'guessed']);
        self::assertResponseStatusCodeSame(403);
        $this->client->request('POST', '/logout', server: ['HTTP_ORIGIN' => 'null']);
        self::assertResponseStatusCodeSame(403);

        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?', $this->authorize(), 'Still signed in.');
    }

    public function testAnotherSiteCannotEndTheSignInOrSpendAttemptsThroughTheSignInForm(): void
    {
        $this->signIn();
        self::assertNotNull($this->client->getCookieJar()->get('REMEMBERME'));

        for ($post = 1; $post <= 6; ++$post) {
            $this->client->request('POST', '/login', ['_username' => self::EMAIL, '_password' => 'wrong', '_csrf_token' => 'forged'], server: ['HTTP_ORIGIN' => 'https://evil.example']);
            self::assertResponseStatusCodeSame(403);
            self::assertSelectorTextContains('.error', 'came from another site');
        }

        self::assertNotNull($this->client->getCookieJar()->get('REMEMBERME'), 'The remember-me cookie stays.');
        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?', $this->authorize(), 'Still signed in.');
        $this->client->getCookieJar()->clear();
        $this->signIn(rememberMe: false);
        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?', $this->authorize(), 'None of those posts counted as a failed attempt.');
    }

    public function testARefusedPostWithoutTheSessionCookieStartsNoSession(): void
    {
        $this->signIn();
        $this->dropSession();

        foreach (['/login', '/logout'] as $path) {
            $this->client->request('POST', $path, ['_username' => self::EMAIL, '_password' => 'x', '_csrf_token' => 'forged'], server: ['HTTP_ORIGIN' => 'https://evil.example']);
            self::assertResponseStatusCodeSame(303, $path);
            self::assertResponseRedirects($path);
            self::assertSame([], $this->client->getResponse()->headers->getCookies(), 'No new session cookie: '.$path);
        }
        self::assertNotNull($this->client->getCookieJar()->get('REMEMBERME'));
    }

    public function testPromptLoginAsksForThePasswordAgainOnceForThatRequest(): void
    {
        $this->signIn();
        $verifier = bin2hex(random_bytes(32));
        $request = $this->desktopAuthorizeUrl($verifier, ['prompt' => 'login']);

        $this->client->request('GET', $request);
        self::assertResponseRedirects('http://localhost/login', null, 'A live session is not enough.');
        self::assertNotNull($this->client->getCookieJar()->get('REMEMBERME'), 'Nothing ends before someone signs in.');
        $this->client->followRedirect();
        $this->client->submit($this->client->getCrawler()->selectButton('Sign in')->form(['_username' => self::EMAIL, '_password' => self::PASSWORD]));
        $back = (string) $this->client->getResponse()->headers->get('Location');
        self::assertStringStartsWith('http://localhost/oauth/authorize?', $back, 'Back to the same request.');
        $this->client->followRedirect();

        $location = (string) $this->client->getResponse()->headers->get('Location');
        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?', $location, 'It goes on without asking again.');

        $this->client->request('GET', $request);
        self::assertResponseRedirects('http://localhost/login', null, 'The answer was for that one request: asking again asks again.');
    }

    public function testSignOutWithoutASignInGoesToTheWebClient(): void
    {
        $this->client->request('GET', '/logout');

        self::assertResponseRedirects('http://localhost:5173');
    }

    public function testAccessTokensCarryTheScopesAskedFor(): void
    {
        $this->signIn();
        $verifier = bin2hex(random_bytes(32));
        $this->client->request('POST', '/oauth/token', $this->codeExchange($this->codeFrom($this->authorizedCallback($this->desktopAuthorizeUrl($verifier, ['scope' => 'sync']))), $verifier));
        self::assertResponseIsSuccessful();

        $this->me(self::string($this->decodedResponse()['access_token'] ?? null));

        self::assertResponseStatusCodeSame(403, 'Reading the account needs the account scope.');
    }

    public function testAnUnknownScopeIsRefused(): void
    {
        $this->signIn();

        $callback = $this->authorizedCallback($this->desktopAuthorizeUrl(bin2hex(random_bytes(32)), ['scope' => 'account admin']));

        parse_str((string) parse_url($callback, \PHP_URL_QUERY), $answer);
        self::assertSame('invalid_scope', $answer['error'] ?? null);
    }

    public function testARefreshTokenRotates(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);

        $this->refresh($tokens['refresh_token']);

        self::assertResponseIsSuccessful();
        $rotated = $this->decodedResponse();
        self::assertNotSame($tokens['refresh_token'], $rotated['refresh_token'] ?? null);
        $this->me(self::string($rotated['access_token'] ?? null));
        self::assertResponseIsSuccessful();
    }

    public function testRevokingTheRefreshTokenEndsTheSignIn(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);

        $this->revoke($tokens['refresh_token'], 'refresh_token');

        self::assertResponseStatusCodeSame(200);
        self::assertSame('', $this->client->getResponse()->getContent());
        self::assertResponseHeaderSame('Cache-Control', 'no-store, private');
        $this->refresh($tokens['refresh_token']);
        self::assertResponseStatusCodeSame(400);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
        $this->me($tokens['access_token']);
        self::assertResponseStatusCodeSame(401, 'The access token issued with it is revoked too.');
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM device'), 'The device is signed out.');
    }

    public function testRevokingTheAccessTokenEndsItsRefreshTokenToo(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);

        $this->revoke($tokens['access_token'], 'access_token');

        self::assertResponseStatusCodeSame(200);
        $this->me($tokens['access_token']);
        self::assertResponseStatusCodeSame(401);
        $this->refresh($tokens['refresh_token']);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
    }

    public function testRevocationWorksWithoutAHintAndTellsNothing(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);

        $this->revoke('not-a-token', null);
        self::assertResponseStatusCodeSame(200);
        $this->revoke($tokens['refresh_token'], 'access_token');
        self::assertResponseStatusCodeSame(200, 'A wrong hint only changes where the server looks first.');
        $this->refresh($tokens['refresh_token']);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
    }

    public function testOnlyTheClientATokenWasIssuedToCanRevokeIt(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);

        $this->revoke($tokens['refresh_token'], 'refresh_token', 'web');

        self::assertResponseStatusCodeSame(200, 'The answer says nothing about the token.');
        $this->refresh($tokens['refresh_token']);
        self::assertResponseIsSuccessful();
    }

    public function testARevocationNeedsATokenAndAClient(): void
    {
        $this->client->request('POST', '/oauth/revoke', ['client_id' => 'desktop']);
        self::assertResponseStatusCodeSame(400);
        self::assertSame('invalid_request', $this->decodedResponse()['error'] ?? null);

        $this->client->request('POST', '/oauth/revoke', ['token' => 'something']);
        self::assertResponseStatusCodeSame(400);
    }

    public function testARefreshTokenThatJustRotatedGetsTheSameAnswerAgain(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);
        $this->refresh($tokens['refresh_token']);
        self::assertResponseIsSuccessful();
        $rotated = $this->decodedResponse();

        $this->refresh($tokens['refresh_token']);

        self::assertResponseIsSuccessful();
        $again = $this->decodedResponse();
        self::assertSame($rotated['refresh_token'] ?? null, $again['refresh_token'] ?? null, 'No second token family.');
        self::assertSame($rotated['access_token'] ?? null, $again['access_token'] ?? null);
        $this->me($tokens['access_token']);
        self::assertResponseIsSuccessful('A request already sent with the old access token still goes through.');
    }

    public function testTheGraceOfARotatedRefreshTokenEnds(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);
        $this->refresh($tokens['refresh_token']);
        $rotated = $this->decodedResponse();

        self::mockTime('+'.(RefreshTokenRotation::GRACE_SECONDS + 1).' seconds');

        $this->me($tokens['access_token']);
        self::assertResponseStatusCodeSame(401, 'The old access token ended with the grace.');
        $this->me(self::string($rotated['access_token'] ?? null));
        self::assertResponseIsSuccessful('The new tokens are unaffected by the grace ending.');
    }

    public function testARefreshTokenUsedAgainAfterTheGraceSignsItsDeviceOut(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);
        $other = $this->signInDevice(self::EMAIL, 'Another computer');
        $this->refresh($tokens['refresh_token']);
        $rotated = $this->decodedResponse();
        self::mockTime('+'.(RefreshTokenRotation::GRACE_SECONDS + 1).' seconds');

        $this->refresh($tokens['refresh_token']);

        self::assertResponseStatusCodeSame(400);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
        $this->me(self::string($rotated['access_token'] ?? null));
        self::assertResponseStatusCodeSame(401, 'Someone holds a copy of the token: what it rotated into ends too.');
        $this->refresh(self::string($rotated['refresh_token'] ?? null));
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM device'), 'Only that device is signed out.');
        $this->me($other['access_token']);
        self::assertResponseIsSuccessful();
    }

    public function testAnExpiredAccessTokenStillSignsOutADeviceThatDidNotRefresh(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);
        self::mockTime('+2 hours');

        $this->revoke($tokens['access_token'], 'access_token');

        $this->refresh($tokens['refresh_token']);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM device'));
    }

    public function testAnExpiredAccessTokenLeftBehindByARefreshRevokesNothing(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);
        $this->refresh($tokens['refresh_token']);
        $rotated = $this->decodedResponse();
        self::mockTime('+2 hours');

        $this->revoke($tokens['access_token'], 'access_token');

        self::assertResponseStatusCodeSame(200, 'The answer says nothing about the token.');
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM device'));
        $this->refresh(self::string($rotated['refresh_token'] ?? null));
        self::assertResponseIsSuccessful('A token from a log does not end a sign-in that moved on.');
    }

    public function testTheDeviceCodeEndpointDoesNotExist(): void
    {
        $this->client->request('POST', '/oauth/device-code', ['client_id' => 'desktop']);

        self::assertResponseStatusCodeSame(404);
    }

    public function testABrowserThatIsSignedInAlreadyIsAskedBeforeTheDesktopAppSignsIn(): void
    {
        $this->signIn();
        $request = $this->desktopAuthorizeUrl(bin2hex(random_bytes(32)), ['state' => 'state-2']);

        $crawler = $this->client->request('GET', $request);

        self::assertResponseIsSuccessful();
        self::assertFalse($this->client->getResponse()->headers->has('Location'), 'Nothing goes to the app by itself.');
        self::assertSelectorTextContains('h1', 'Sign in to Varve on this computer?');
        self::assertSelectorTextContains('.lead', self::EMAIL);
        self::assertResponseHeaderSame('X-Frame-Options', 'DENY');
        self::assertResponseHeaderSame('Referrer-Policy', 'no-referrer');
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM oauth2_authorization_code'));

        $this->client->submit($crawler->selectButton('Cancel')->form());
        $cancelled = (string) $this->client->getResponse()->headers->get('Location');
        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?', $cancelled);
        parse_str((string) parse_url($cancelled, \PHP_URL_QUERY), $answer);
        self::assertSame('access_denied', $answer['error'] ?? null);
        self::assertSame('state-2', $answer['state'] ?? null);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM oauth2_authorization_code'));

        $crawler = $this->client->request('GET', $request);
        $this->client->submit($crawler->selectButton('Continue')->form());
        $this->codeFrom((string) $this->client->getResponse()->headers->get('Location'));
    }

    public function testAnAnswerFromAnotherSiteSignsNothingIn(): void
    {
        $this->signIn();
        $request = $this->desktopAuthorizeUrl(bin2hex(random_bytes(32)));
        $this->client->request('GET', $request);

        foreach (['forged', ''] as $token) {
            $this->client->request('POST', $request, ['decision' => 'continue', '_csrf_token' => $token]);

            self::assertResponseStatusCodeSame(403);
            self::assertSelectorTextContains('.error', 'came from another site');
        }
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM oauth2_authorization_code'));
    }

    public function testASignInAnswersOnlyTheRequestItWasFor(): void
    {
        $first = $this->desktopAuthorizeUrl(bin2hex(random_bytes(32)));
        $other = $this->desktopAuthorizeUrl(bin2hex(random_bytes(32)));
        $this->client->request('GET', $first);
        $crawler = $this->client->followRedirect();
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => self::EMAIL, '_password' => self::PASSWORD]));

        $this->client->request('GET', $other);
        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Sign in to Varve on this computer?', 'Another request, which another program may have made.');

        $this->client->request('GET', $first);
        self::assertStringStartsWith(self::DESKTOP_REDIRECT_URI.'?code=', (string) $this->client->getResponse()->headers->get('Location'));

        $this->client->request('GET', $first);
        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Sign in to Varve on this computer?', 'The sign-in answered that request once.');
    }

    public function testARegisteredClientThatIsNotOursIsDenied(): void
    {
        $client = new Client('Someone else\'s app', 'someone-elses-app', null);
        $client->setRedirectUris(new RedirectUri('http://localhost:9999/callback'));
        $client->setGrants(new Grant(OAuth2Grants::AUTHORIZATION_CODE));
        static::getContainer()->get(ClientManagerInterface::class)->save($client);
        $this->signIn();

        $this->client->request('GET', $this->desktopAuthorizeUrl(bin2hex(random_bytes(32)), ['client_id' => 'someone-elses-app', 'redirect_uri' => 'http://localhost:9999/callback']));

        $callback = (string) $this->client->getResponse()->headers->get('Location');
        self::assertStringStartsWith('http://localhost:9999/callback?', $callback);
        parse_str((string) parse_url($callback, \PHP_URL_QUERY), $answer);
        self::assertSame('access_denied', $answer['error'] ?? null);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM oauth2_authorization_code'));
    }

    public function testTheAddressSignsInHoweverItIsCapitalized(): void
    {
        $crawler = $this->client->request('GET', '/login');
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => 'Kim@Example.TEST', '_password' => self::PASSWORD]));

        self::assertResponseRedirects('http://localhost/signed-in');
        $this->client->followRedirect();
        self::assertSelectorTextContains('.lead', self::EMAIL);
    }

    public function testWrongPasswordsCountForAWholeIpv6Network(): void
    {
        for ($attempt = 1; $attempt <= 5; ++$attempt) {
            $this->client->setServerParameter('REMOTE_ADDR', '2001:db8:1:2::'.$attempt);
            $this->signIn(password: 'wrong-'.$attempt);
        }

        $this->client->setServerParameter('REMOTE_ADDR', '2001:db8:1:2:ffff::9');
        $this->signIn();
        $this->client->followRedirect();
        self::assertSelectorTextContains('.error', 'Too many failed login attempts', 'A new address of the same /64 is the same network.');

        $this->client->setServerParameter('REMOTE_ADDR', '2001:db8:1:3::1');
        $this->signIn();
        self::assertResponseRedirects('http://localhost/signed-in', null, 'Another network is not locked.');
    }

    public function testWrongPasswordsForOneAccountCountWhateverTheNetwork(): void
    {
        for ($attempt = 1; $attempt <= 20; ++$attempt) {
            $this->client->setServerParameter('REMOTE_ADDR', '203.0.113.'.$attempt);
            $this->signIn(password: 'wrong-'.$attempt);
            $this->client->followRedirect();
            self::assertSelectorTextContains('.error', 'Invalid credentials', 'Attempt '.$attempt);
        }

        $this->client->setServerParameter('REMOTE_ADDR', '198.51.100.7');
        $this->signIn();
        $this->client->followRedirect();
        self::assertSelectorTextContains('.error', 'Too many failed login attempts');

        $this->createUser('lisa@example.test', 'Lisa Bakker');
        $this->client->setServerParameter('REMOTE_ADDR', '198.51.100.8');
        $crawler = $this->client->request('GET', '/login');
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => 'lisa@example.test', '_password' => self::PASSWORD]));
        self::assertResponseRedirects('http://localhost/signed-in', null, 'Another account is not locked.');
    }

    public function testRevokingTheOldTokenAlsoEndsWhatItJustRotatedInto(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);
        $this->refresh($tokens['refresh_token']);
        $lost = $this->decodedResponse();

        $this->revoke($tokens['refresh_token'], 'refresh_token');

        $this->refresh(self::string($lost['refresh_token'] ?? null));
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null, 'The response that never arrived is revoked with it.');
        $this->me(self::string($lost['access_token'] ?? null));
        self::assertResponseStatusCodeSame(401);
    }

    private function revoke(string $token, ?string $hint, string $clientId = 'desktop'): void
    {
        $this->client->request('POST', '/oauth/revoke', array_filter(['token' => $token, 'token_type_hint' => $hint, 'client_id' => $clientId], static fn (?string $value): bool => null !== $value));
    }

    private function signIn(bool $rememberMe = true, string $password = self::PASSWORD): void
    {
        $crawler = $this->client->request('GET', '/login');
        $form = $crawler->selectButton('Sign in')->form(['_username' => self::EMAIL, '_password' => $password]);
        if (!$rememberMe) {
            $checkbox = $form['_remember_me'];
            self::assertInstanceOf(ChoiceFormField::class, $checkbox);
            $checkbox->untick();
        }
        $this->client->submit($form);
    }

    /** The browser's session cookie is gone (it closed, or the session expired); other cookies stay. */
    private function dropSession(): void
    {
        // The name of the session cookie in tests (MockFileSessionStorage).
        $this->client->getCookieJar()->expire('MOCKSESSID');
        self::assertNull($this->client->getCookieJar()->get('MOCKSESSID'));
    }

    /**
     * Where /oauth/authorize sends the browser for the desktop app: the sign-in page, or, for a browser that is signed
     * in and answers Continue, the callback with a code.
     */
    private function authorize(): string
    {
        $location = $this->authorizedCallback($this->desktopAuthorizeUrl(bin2hex(random_bytes(32))));
        self::assertResponseRedirects();

        return $location;
    }
}
