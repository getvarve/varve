<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Doctrine\ORM\EntityManagerInterface;
use Symfony\Component\Clock\Test\ClockSensitiveTrait;
use Symfony\Component\DomCrawler\Field\ChoiceFormField;

/**
 * A forgotten password: the link, the new password, and everything a reset ends.
 */
final class PasswordResetTest extends DatabaseTestCase
{
    use AuthenticatesUsers;
    use ClockSensitiveTrait;
    use ReadsAccountMail;

    private const string EMAIL = 'kim@example.test';
    private const string NEW_PASSWORD = 'a-brand-new-password';

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->createUser(self::EMAIL, 'Kim Visser');
    }

    public function testAResetSetsTheNewPasswordAndSignsOutEveryDevice(): void
    {
        $tokens = $this->signInDevice(self::EMAIL);
        $this->signInBrowser();

        $this->askForLink(self::EMAIL);
        self::assertSelectorTextContains('h1', 'Check your mail');
        $email = $this->sentMail(self::EMAIL);
        self::assertSame('Reset your Varve password', $email->getSubject());
        $secret = $this->linkSecret($email, '/reset-password');

        $crawler = $this->client->request('GET', '/reset-password?token='.$secret);
        self::assertResponseHeaderSame('Referrer-Policy', 'no-referrer');
        $this->client->submit($crawler->selectButton('Set password')->form(['password' => self::NEW_PASSWORD]));
        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Password set');

        $this->me($tokens['access_token']);
        self::assertResponseStatusCodeSame(401, 'The device that knew the old password is out.');
        $this->refresh($tokens['refresh_token']);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM device'));

        $this->signInDevice(self::EMAIL, password: self::NEW_PASSWORD);
        self::assertResponseIsSuccessful();
    }

    public function testAResetEndsTheRememberedBrowserSignIn(): void
    {
        $this->signInBrowser();
        $remembered = $this->client->getCookieJar()->get('REMEMBERME');
        self::assertNotNull($remembered);

        $this->askForLink(self::EMAIL);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/reset-password');
        $this->client->getCookieJar()->clear();
        $crawler = $this->client->request('GET', '/reset-password?token='.$secret);
        $this->client->submit($crawler->selectButton('Set password')->form(['password' => self::NEW_PASSWORD]));
        static::getContainer()->get(EntityManagerInterface::class)->clear();

        $this->client->getCookieJar()->clear();
        $this->client->getCookieJar()->set($remembered);
        $this->client->request('GET', $this->desktopAuthorizeUrl(bin2hex(random_bytes(32))));
        self::assertResponseRedirects('http://localhost/login', null, 'The remember-me cookie was signed with the old password.');
    }

    public function testAResetConfirmsAnUnconfirmedAddress(): void
    {
        $this->connection()->executeStatement('UPDATE app_user SET email_confirmed_at = NULL');
        static::getContainer()->get(EntityManagerInterface::class)->clear();

        $this->askForLink(self::EMAIL);
        $this->setPassword($this->linkSecret($this->sentMail(self::EMAIL), '/reset-password'), self::NEW_PASSWORD);

        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));
        $this->signInDevice(self::EMAIL, password: self::NEW_PASSWORD);
    }

    public function testAnAddressWithoutAnAccountGetsTheSamePageAndNoMail(): void
    {
        $this->askForLink('nobody@example.test');

        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Check your mail');
        self::assertEmailCount(0);
    }

    public function testAResetLinkWorksOnce(): void
    {
        $this->askForLink(self::EMAIL);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/reset-password');
        $csrf = $this->resetCsrf($secret);
        $this->setPassword($secret, self::NEW_PASSWORD);

        $this->client->request('GET', '/reset-password?token='.$secret);
        self::assertSelectorTextContains('h1', 'Link no longer works');
        $this->client->request('POST', '/reset-password', ['token' => $secret, 'password' => 'yet-another-password', '_csrf_token' => $csrf]);
        self::assertResponseStatusCodeSame(410);
        $this->signInDevice(self::EMAIL, password: self::NEW_PASSWORD);
    }

    public function testAnExpiredResetLinkDoesNotWork(): void
    {
        $this->askForLink(self::EMAIL);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/reset-password');
        $csrf = $this->resetCsrf($secret);

        self::mockTime('+1 hour +1 second');
        $this->client->request('POST', '/reset-password', ['token' => $secret, 'password' => self::NEW_PASSWORD, '_csrf_token' => $csrf]);

        self::assertResponseStatusCodeSame(410);
        $this->signInDevice(self::EMAIL);
    }

    public function testOnlyTheLatestResetLinkWorks(): void
    {
        $this->askForLink(self::EMAIL);
        $first = $this->linkSecret($this->sentMail(self::EMAIL), '/reset-password');
        $this->askForLink(self::EMAIL);
        $second = $this->linkSecret($this->sentMail(self::EMAIL), '/reset-password');
        $csrf = $this->resetCsrf($second);

        $this->client->request('POST', '/reset-password', ['token' => $first, 'password' => self::NEW_PASSWORD, '_csrf_token' => $csrf]);

        self::assertResponseStatusCodeSame(410);
    }

    public function testTheNewPasswordMustBeLongEnough(): void
    {
        $this->askForLink(self::EMAIL);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/reset-password');

        $crawler = $this->client->request('GET', '/reset-password?token='.$secret);
        $this->client->submit($crawler->selectButton('Set password')->form(['password' => 'too-short']));

        self::assertResponseStatusCodeSame(422);
        self::assertSelectorTextContains('#password-error', 'at least 12 characters');
        $this->setPassword($secret, self::NEW_PASSWORD);
        self::assertSelectorTextContains('h1', 'Password set', 'The link still works after a refused password.');
    }

    public function testAResetFromAnotherSiteDoesNothing(): void
    {
        $this->askForLink(self::EMAIL);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/reset-password');

        $this->client->request('POST', '/reset-password', ['token' => $secret, 'password' => self::NEW_PASSWORD, '_csrf_token' => 'forged']);
        self::assertResponseStatusCodeSame(403);

        // Without the session cookie (as another site's form sends it) nothing is rendered: back to the page.
        $this->client->getCookieJar()->clear();
        $this->client->request('POST', '/reset-password', ['token' => $secret, 'password' => self::NEW_PASSWORD, '_csrf_token' => 'forged']);
        self::assertResponseStatusCodeSame(303);
        self::assertSame([], $this->client->getResponse()->headers->getCookies());

        $this->signInDevice(self::EMAIL);
    }

    public function testAResetEndsTheLiveBrowserSession(): void
    {
        $this->signInBrowser(rememberMe: false);
        self::assertNull($this->client->getCookieJar()->get('REMEMBERME'));
        $session = $this->client->getCookieJar()->get('MOCKSESSID');
        self::assertNotNull($session);

        // The reset happens in another browser.
        $this->client->getCookieJar()->clear();
        $this->askForLink(self::EMAIL);
        $this->setPassword($this->linkSecret($this->sentMail(self::EMAIL), '/reset-password'), self::NEW_PASSWORD);
        static::getContainer()->get(EntityManagerInterface::class)->clear();

        $this->client->getCookieJar()->clear();
        $this->client->getCookieJar()->set($session);
        $this->client->request('GET', $this->desktopAuthorizeUrl(bin2hex(random_bytes(32))));
        self::assertResponseRedirects('http://localhost/login', null, 'The session was for the old password.');
    }

    public function testTheResetMailGreetsAConfirmedAccountByName(): void
    {
        $this->askForLink(self::EMAIL);

        self::assertStringContainsString('Hi Kim Visser,', (string) $this->sentMail(self::EMAIL)->getTextBody());
    }

    public function testRequestsFromOneNetworkAreLimitedWhateverTheAddress(): void
    {
        for ($ask = 1; $ask <= 20; ++$ask) {
            $this->askForLink('nobody'.$ask.'@example.test');
            self::assertResponseIsSuccessful();
        }

        $this->askForLink(self::EMAIL);

        self::assertSelectorTextContains('h1', 'Check your mail', 'The page says the same.');
        self::assertEmailCount(0, message: 'Asking about addresses without an account counted too.');
    }

    public function testSomeoneElsesRequestsDoNotUseUpTheOwnersLinks(): void
    {
        $this->client->setServerParameter('REMOTE_ADDR', '203.0.113.66');
        for ($ask = 1; $ask <= 5; ++$ask) {
            $this->askForLink(self::EMAIL);
            self::assertEmailCount(1, message: 'Mail '.$ask);
        }
        $this->askForLink(self::EMAIL);
        self::assertEmailCount(0, message: 'That network has had its share for this address.');

        $this->client->setServerParameter('REMOTE_ADDR', '198.51.100.20');
        $this->askForLink(self::EMAIL);
        self::assertEmailCount(1, message: 'The owner, on another network, still gets a link.');
    }

    public function testMailOfAnotherKindDoesNotUseUpResetLinks(): void
    {
        for ($signUp = 1; $signUp <= 6; ++$signUp) {
            $crawler = $this->client->request('GET', '/register');
            $this->client->submit($crawler->selectButton('Create account')->form(['email' => self::EMAIL, 'name' => 'Someone Else', 'password' => 'another-password-entirely']));
            self::assertEmailCount($signUp <= 5 ? 1 : 0, message: 'The note that the address has an account, '.$signUp);
        }

        $this->askForLink(self::EMAIL);

        self::assertEmailCount(1);
    }

    public function testASignedInBrowserGetsALinkWhateverOthersAskedFor(): void
    {
        for ($ask = 1; $ask <= 6; ++$ask) {
            $this->askForLink(self::EMAIL);
        }
        self::assertEmailCount(0, message: 'The budget of this network for this address is used up.');

        $this->signInBrowser();
        $this->askForLink('Kim@Example.test');
        $this->setPassword($this->linkSecret($this->sentMail(self::EMAIL), '/reset-password'), self::NEW_PASSWORD);

        $this->askForLink('someone-else@example.test');
        self::assertEmailCount(0, message: 'Being signed in says nothing about another address.');
    }

    public function testTheHolderOfAnExpiredLinkGetsANewOneWhateverOthersAskedFor(): void
    {
        $secret = '';
        for ($ask = 1; $ask <= 5; ++$ask) {
            $this->askForLink(self::EMAIL);
            $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/reset-password');
        }
        $this->askForLink(self::EMAIL);
        self::assertEmailCount(0, message: 'The budget of this network for this address is used up.');

        self::mockTime('+1 hour +1 second');
        $crawler = $this->client->request('GET', '/reset-password?token='.$secret);
        self::assertSelectorTextContains('h1', 'Link expired');
        $this->client->submit($crawler->selectButton('Send a new link')->form());

        self::assertSelectorTextContains('h1', 'Check your mail');
        $this->setPassword($this->linkSecret($this->sentMail(self::EMAIL), '/reset-password'), self::NEW_PASSWORD);
    }

    public function testAMadeUpLinkGetsNobodyANewOne(): void
    {
        $crawler = $this->client->request('GET', '/reset-password?token='.str_repeat('a', 64));
        self::assertSelectorTextContains('h1', 'Link no longer works');
        self::assertSame(0, $crawler->selectButton('Send a new link')->count());

        $crawler = $this->client->request('GET', '/forgot-password');
        $this->client->request('POST', '/forgot-password', ['link' => str_repeat('a', 64), '_csrf_token' => self::string($crawler->filter('input[name="_csrf_token"]')->attr('value'))]);

        self::assertSelectorTextContains('h1', 'Check your mail', 'The page says the same.');
        self::assertEmailCount(0);
    }

    private function askForLink(string $email): void
    {
        $crawler = $this->client->request('GET', '/forgot-password');
        $this->client->submit($crawler->selectButton('Send link')->form(['email' => $email]));
    }

    private function setPassword(string $secret, string $password): void
    {
        $crawler = $this->client->request('GET', '/reset-password?token='.$secret);
        $this->client->submit($crawler->selectButton('Set password')->form(['password' => $password]));
        self::assertResponseIsSuccessful();
    }

    private function resetCsrf(string $workingSecret): string
    {
        $crawler = $this->client->request('GET', '/reset-password?token='.$workingSecret);

        return self::string($crawler->filter('input[name="_csrf_token"]')->attr('value'));
    }

    private function signInBrowser(bool $rememberMe = true): void
    {
        $crawler = $this->client->request('GET', '/login');
        $form = $crawler->selectButton('Sign in')->form(['_username' => self::EMAIL, '_password' => self::DEFAULT_PASSWORD]);
        if (!$rememberMe) {
            $checkbox = $form['_remember_me'];
            self::assertInstanceOf(ChoiceFormField::class, $checkbox);
            $checkbox->untick();
        }
        $this->client->submit($form);
    }
}
