<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Account\PurgeAbandonedSignUps;
use App\Account\PurgeAbandonedSignUpsHandler;
use App\Entity\User;
use Symfony\Component\Clock\Test\ClockSensitiveTrait;

/**
 * Signing up in the browser, confirming the address, and what the pages tell (or do not tell) about who has an
 * account.
 */
final class RegistrationTest extends DatabaseTestCase
{
    use AuthenticatesUsers;
    use ClockSensitiveTrait;
    use ReadsAccountMail;

    private const string EMAIL = 'noor@example.test';
    private const string PASSWORD = 'a-long-enough-password';
    private const string OTHER_PASSWORD = 'the-password-of-someone-else';

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->resetRateLimits();
    }

    public function testSignUpConfirmAndSignIn(): void
    {
        $this->register(self::EMAIL, 'Noor de Vries', self::PASSWORD);

        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Check your mail');
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'noor@example.test' AND email_confirmed_at IS NULL"));
        self::assertSame(1, $this->countRows(
            "SELECT COUNT(*) FROM organization o JOIN organization_member m ON m.organization_id = o.id JOIN app_user u ON u.id = o.personal_owner_id
             WHERE u.email = 'noor@example.test' AND m.user_id = u.id AND m.role = 'owner'",
        ), 'The personal organization exists from the start, with its owner as its one seat.');

        $this->client->request('GET', '/confirm-email?token='.$secret);
        self::assertResponseIsSuccessful();
        self::assertResponseHeaderSame('Referrer-Policy', 'no-referrer');
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'), 'Opening the link (as a mail scanner does) confirms nothing.');
        $this->client->submit($this->client->getCrawler()->selectButton('Confirm')->form(['password' => self::PASSWORD]));
        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Address confirmed');
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM account_link'));

        $this->me($this->signInDevice(self::EMAIL, password: self::PASSWORD)['access_token']);
        self::assertResponseIsSuccessful();
        self::assertSame('Noor de Vries', $this->decodedResponse()['displayName'] ?? null);
    }

    public function testConfirmingNeedsThePasswordOfTheSignUp(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $csrf = $this->confirmCsrf($secret);

        $this->confirm($secret, $csrf, 'not-the-password-i-chose');

        self::assertResponseStatusCodeSame(422);
        self::assertSelectorTextContains('#password-error', 'not the password you chose');
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));
        $this->confirm($secret, $csrf);
        self::assertSelectorTextContains('h1', 'Address confirmed', 'The link still works after a wrong password.');
    }

    public function testAConfirmationLinkWorksOnce(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $csrf = $this->confirmCsrf($secret);
        $this->confirm($secret, $csrf);
        self::assertResponseIsSuccessful();

        $this->client->request('GET', '/confirm-email?token='.$secret);
        self::assertSelectorTextContains('h1', 'Link no longer works');
        $this->confirm($secret, $csrf);
        self::assertResponseStatusCodeSame(410);
    }

    public function testAnExpiredConfirmationLinkDoesNotWorkButGetsItsHolderANewOne(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $csrf = $this->confirmCsrf($secret);

        self::mockTime('+2 days +1 second');
        $this->confirm($secret, $csrf);

        self::assertResponseStatusCodeSame(410);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));

        $crawler = $this->client->request('GET', '/confirm-email?token='.$secret);
        self::assertSelectorTextContains('h1', 'Link expired');
        $this->client->submit($crawler->selectButton('Send a new link')->form());
        self::assertSelectorTextContains('h1', 'Check your mail');
        $new = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $this->confirm($new, $this->confirmCsrf($new));
        self::assertSelectorTextContains('h1', 'Address confirmed', 'The new link is for the same sign-up.');
    }

    public function testAMadeUpLinkDoesNotWork(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $csrf = $this->confirmCsrf($this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email'));

        foreach ([str_repeat('a', 64), 'short', ''] as $secret) {
            $this->confirm($secret, $csrf);
            self::assertResponseStatusCodeSame(410, $secret);
        }
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));

        $crawler = $this->client->request('GET', '/confirm-email/resend');
        $this->client->request('POST', '/confirm-email/resend', ['link' => str_repeat('a', 64), '_csrf_token' => self::string($crawler->filter('input[name="_csrf_token"]')->attr('value'))]);
        self::assertSelectorTextContains('h1', 'Check your mail', 'The page says the same.');
        self::assertEmailCount(0);
    }

    public function testAConfirmationFromAnotherSiteDoesNothing(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');

        $this->confirm($secret, 'forged');

        self::assertResponseStatusCodeSame(403);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));
    }

    public function testAPostWithoutTheSessionCookieStartsNoSession(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $this->client->getCookieJar()->clear();

        foreach ([
            '/register' => ['email' => 'other@example.test', 'name' => 'Other', 'password' => self::PASSWORD],
            '/confirm-email/resend' => ['email' => self::EMAIL],
            '/forgot-password' => ['email' => self::EMAIL],
        ] as $path => $fields) {
            $this->client->request('POST', $path, [...$fields, '_csrf_token' => 'forged'], server: ['HTTP_ORIGIN' => 'https://evil.example']);
            self::assertResponseStatusCodeSame(303, $path);
            self::assertResponseRedirects($path);
            self::assertSame([], $this->client->getResponse()->headers->getCookies(), 'No new session cookie: '.$path);
            self::assertEmailCount(0);
        }
        foreach (['/confirm-email', '/reset-password'] as $path) {
            $this->client->request('POST', $path, ['token' => $secret, 'password' => self::PASSWORD, '_csrf_token' => 'forged']);
            self::assertResponseStatusCodeSame(303, $path);
            self::assertResponseRedirects($path.'?token='.$secret, null, 'Back to the page of the link.');
            self::assertSame([], $this->client->getResponse()->headers->getCookies(), 'No new session cookie: '.$path);
        }
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM app_user'));
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));
    }

    public function testOnlyTheLatestConfirmationLinkWorks(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $first = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');

        $this->resend(self::EMAIL);
        $second = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $csrf = $this->confirmCsrf($second);

        $this->confirm($first, $csrf);
        self::assertResponseStatusCodeSame(410);
        $this->confirm($second, $csrf);
        self::assertResponseIsSuccessful();
    }

    public function testSigningUpAgainWithAConfirmedAddressChangesNothingAndSaysNothing(): void
    {
        $this->createUser(self::EMAIL, 'Noor');

        $this->register(self::EMAIL, 'Someone Else', 'another-password-entirely');

        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Check your mail', 'The same page as for a new address.');
        $email = $this->sentMail(self::EMAIL);
        self::assertSame('You already have a Varve account', $email->getSubject());
        self::assertStringContainsString($this->apiAddress().'/forgot-password', (string) $email->getTextBody());
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM account_link'));
        $this->me($this->signInDevice(self::EMAIL)['access_token']);
        self::assertSame('Noor', $this->decodedResponse()['displayName'] ?? null, 'The name and the password stay.');
    }

    /** Replaces the test that pinned a second sign-up overwriting the name and password of the unconfirmed account. */
    public function testEveryConfirmationLinkConfirmsOnlyItsOwnSignUp(): void
    {
        $this->register(self::EMAIL, 'Someone Else', self::OTHER_PASSWORD);
        $first = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');

        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $second = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $csrf = $this->confirmCsrf($second);

        $this->confirm($first, $csrf, self::OTHER_PASSWORD);
        self::assertResponseStatusCodeSame(410, 'The first sign-up\'s link ended.');
        $this->confirm($second, $csrf, self::OTHER_PASSWORD);
        self::assertResponseStatusCodeSame(422, 'The second link is not for the first sign-up\'s password.');
        $this->confirm($second, $csrf);
        self::assertSelectorTextContains('h1', 'Address confirmed');

        $this->me($this->signInDevice(self::EMAIL, password: self::PASSWORD)['access_token']);
        self::assertSame('Noor', $this->decodedResponse()['displayName'] ?? null);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization'));
        $this->assertCannotSignIn(self::EMAIL, self::OTHER_PASSWORD);
    }

    public function testTheOwnerOfTheMailboxCannotConfirmSomeoneElsesSignUpByMistake(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $this->sentMail(self::EMAIL);
        $this->register(self::EMAIL, 'Someone Else', self::OTHER_PASSWORD);
        $theirs = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');

        // Noor opens the mail that works, which is the other sign-up's, and types her own password.
        $this->confirm($theirs, $this->confirmCsrf($theirs));

        self::assertResponseStatusCodeSame(422);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));
        $this->assertCannotSignIn(self::EMAIL, self::OTHER_PASSWORD);
    }

    public function testASignUpThatMayNotMailChangesNothing(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        for ($ask = 2; $ask <= 5; ++$ask) {
            $this->resend(self::EMAIL);
            $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        }

        $this->register(self::EMAIL, 'Someone Else', self::OTHER_PASSWORD);

        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Check your mail', 'The page says the same.');
        self::assertEmailCount(0);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM account_link'), 'No link ended and none was made.');
        $this->confirm($secret, $this->confirmCsrf($secret));
        self::assertSelectorTextContains('h1', 'Address confirmed', 'Her own link still confirms her own sign-up.');
        $this->me($this->signInDevice(self::EMAIL, password: self::PASSWORD)['access_token']);
        self::assertSame('Noor', $this->decodedResponse()['displayName'] ?? null);
        $this->assertCannotSignIn(self::EMAIL, self::OTHER_PASSWORD);
    }

    public function testAResetEndsTheConfirmationLinksOfEarlierSignUps(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $confirm = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $csrf = $this->confirmCsrf($confirm);

        $crawler = $this->client->request('GET', '/forgot-password');
        $this->client->submit($crawler->selectButton('Send link')->form(['email' => self::EMAIL]));
        $email = $this->sentMail(self::EMAIL);
        self::assertStringNotContainsString('Noor', (string) $email->getTextBody(), 'No name before the address is confirmed.');
        $crawler = $this->client->request('GET', '/reset-password?token='.$this->linkSecret($email, '/reset-password'));
        $this->client->submit($crawler->selectButton('Set password')->form(['password' => 'the-password-of-the-reset']));
        self::assertSelectorTextContains('h1', 'Password set');

        $this->confirm($confirm, $csrf);
        self::assertResponseStatusCodeSame(410);
        $this->signInDevice(self::EMAIL, password: 'the-password-of-the-reset');
        $this->assertCannotSignIn(self::EMAIL, self::PASSWORD);
    }

    public function testTheConfirmationMailRepeatsNothingTheSignUpChose(): void
    {
        $this->register(self::EMAIL, 'Your account is locked, sign in at https://varve-support.example now', self::PASSWORD);

        $email = $this->sentMail(self::EMAIL);
        foreach ([(string) $email->getTextBody(), (string) $email->getHtmlBody(), (string) $email->getSubject()] as $part) {
            self::assertStringNotContainsString('varve-support.example', $part);
            self::assertStringNotContainsString('locked', $part);
        }
    }

    public function testTheFormChecksWhatItIsGiven(): void
    {
        $this->register('not-an-address', '', 'short');

        self::assertResponseStatusCodeSame(422);
        self::assertSelectorTextContains('#email-error', 'Enter a valid email address.');
        self::assertSelectorTextContains('#name-error', 'Enter your name');
        self::assertSelectorTextContains('#password-error', 'at least 12 characters');
        self::assertEmailCount(0);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    public function testAnAddressTheMailerCannotSendToIsRefused(): void
    {
        foreach (['a..b@example.test', '.a@example.test', 'a.@example.test'] as $address) {
            $this->register($address, 'Noor', self::PASSWORD);

            self::assertResponseStatusCodeSame(422, $address);
            self::assertSelectorTextContains('#email-error', 'Enter a valid email address.');
        }
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    public function testAnAddressIsAtMostAsLongAsTheTokenTablesHold(): void
    {
        $longest = 'noor@'.str_repeat('x', 58).'.'.str_repeat('y', 59).'.test';
        self::assertSame(User::MAX_EMAIL, \strlen($longest));

        $this->register('n'.$longest, 'Noor', self::PASSWORD);
        self::assertResponseStatusCodeSame(422);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user'));

        $this->register($longest, 'Noor', self::PASSWORD);
        self::assertResponseIsSuccessful();
        $secret = $this->linkSecret($this->sentMail($longest), '/confirm-email');
        $this->confirm($secret, $this->confirmCsrf($secret));
        $this->me($this->signInDevice($longest, password: self::PASSWORD)['access_token']);
        self::assertResponseIsSuccessful('The longest address signs in an app.');
    }

    public function testASignUpFromAnotherSiteDoesNothing(): void
    {
        $this->client->request('GET', '/register');
        $this->client->request('POST', '/register', ['email' => self::EMAIL, 'name' => 'Noor', 'password' => self::PASSWORD, '_csrf_token' => 'forged']);

        self::assertResponseStatusCodeSame(403);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    public function testSignUpsFromOneNetworkAreLimited(): void
    {
        for ($signUp = 1; $signUp <= 10; ++$signUp) {
            $this->register('person'.$signUp.'@example.test', 'Person '.$signUp, self::PASSWORD);
            self::assertResponseIsSuccessful('Sign-up '.$signUp);
        }

        $this->register('one-too-many@example.test', 'Person 11', self::PASSWORD);

        self::assertResponseStatusCodeSame(429);
        self::assertSame(0, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'one-too-many@example.test'"));
    }

    public function testMailToOneAddressIsLimited(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $this->sentMail(self::EMAIL);
        for ($ask = 2; $ask <= 5; ++$ask) {
            $this->resend(self::EMAIL);
            self::assertEmailCount(1, message: 'Mail '.$ask);
        }

        $this->resend(self::EMAIL);

        self::assertResponseIsSuccessful('The page says the same.');
        self::assertSelectorTextContains('h1', 'Check your mail');
        self::assertEmailCount(0);

        $this->client->setServerParameter('REMOTE_ADDR', '203.0.113.9');
        $this->resend(self::EMAIL);
        self::assertEmailCount(1, message: 'What one network asked for does not use up what another may ask for.');
    }

    public function testRequestsFromOneNetworkAreLimitedWhateverTheAddress(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        for ($ask = 2; $ask <= 20; ++$ask) {
            $this->resend('nobody'.$ask.'@example.test');
            self::assertResponseIsSuccessful();
        }

        $this->resend(self::EMAIL);

        self::assertSelectorTextContains('h1', 'Check your mail', 'The page says the same.');
        self::assertEmailCount(0, message: 'Asking about addresses without an account counted too.');

        $this->client->setServerParameter('REMOTE_ADDR', '203.0.113.9');
        $this->resend(self::EMAIL);
        self::assertEmailCount(1);
    }

    public function testAskingForALinkForAnAddressWithoutAnAccountSaysTheSame(): void
    {
        $this->resend('nobody@example.test');

        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Check your mail');
        self::assertEmailCount(0);
    }

    public function testAnAccountNeverConfirmedGoesAfterAWeek(): void
    {
        $this->createUser('kept@example.test');
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $handler = static::getContainer()->get(PurgeAbandonedSignUpsHandler::class);

        self::mockTime('+3 days');
        self::assertSame(0, $handler(new PurgeAbandonedSignUps()), 'The expired link stays a while: its holder may ask for a new one.');
        self::assertSame(2, $this->countRows('SELECT COUNT(*) FROM app_user'));

        // Five days after the three above: eight in all.
        self::mockTime('+5 days');
        self::assertSame(1, $handler(new PurgeAbandonedSignUps()));

        self::assertSame(0, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'noor@example.test'"));
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM account_link'), 'Its link went with it.');
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization'), 'Its organization went with it.');
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'kept@example.test'"));
    }

    public function testAnExpiredLinkGoesAWeekAfterItExpired(): void
    {
        $this->createUser('kept@example.test');
        $crawler = $this->client->request('GET', '/forgot-password');
        $this->client->submit($crawler->selectButton('Send link')->form(['email' => 'kept@example.test']));
        $handler = static::getContainer()->get(PurgeAbandonedSignUpsHandler::class);

        self::mockTime('+7 days');
        self::assertSame(0, $handler(new PurgeAbandonedSignUps()));
        // After the seven days above.
        self::mockTime('+1 hour +1 second');
        self::assertSame(1, $handler(new PurgeAbandonedSignUps()));
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM account_link'));
    }

    private function register(string $email, string $name, string $password): void
    {
        $crawler = $this->client->request('GET', '/register');
        $this->client->submit($crawler->selectButton('Create account')->form(['email' => $email, 'name' => $name, 'password' => $password]));
    }

    private function resend(string $email): void
    {
        $crawler = $this->client->request('GET', '/confirm-email/resend');
        $this->client->submit($crawler->selectButton('Send link')->form(['email' => $email]));
    }

    /** The CSRF token of the confirmation form, as the page of a working link shows it. */
    private function confirmCsrf(string $workingSecret): string
    {
        $crawler = $this->client->request('GET', '/confirm-email?token='.$workingSecret);

        return self::string($crawler->filter('input[name="_csrf_token"]')->attr('value'));
    }

    private function confirm(string $secret, string $csrf, string $password = self::PASSWORD): void
    {
        $this->client->request('POST', '/confirm-email', ['token' => $secret, 'password' => $password, '_csrf_token' => $csrf]);
    }

    private function assertCannotSignIn(string $email, string $password): void
    {
        $this->client->getCookieJar()->clear();
        $crawler = $this->client->request('GET', '/login');
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => $email, '_password' => $password]));
        self::assertResponseRedirects('http://localhost/login');
        $this->resetRateLimits();
    }
}
