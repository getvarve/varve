<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Account\PurgeAbandonedSignUps;
use App\Account\PurgeAbandonedSignUpsHandler;
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
        self::assertSame(0, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL"), 'Opening the link (as a mail scanner does) confirms nothing.');
        $this->client->submit($this->client->getCrawler()->selectButton('Confirm')->form());
        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Address confirmed');

        $this->me($this->signInDevice(self::EMAIL, password: self::PASSWORD)['access_token']);
        self::assertResponseIsSuccessful();
        self::assertSame('Noor de Vries', $this->decodedResponse()['displayName'] ?? null);
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

    public function testAnExpiredConfirmationLinkDoesNotWork(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $csrf = $this->confirmCsrf($secret);

        self::mockTime('+2 days +1 second');
        $this->confirm($secret, $csrf);

        self::assertResponseStatusCodeSame(410);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));
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
    }

    public function testAConfirmationFromAnotherSiteDoesNothing(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');

        $this->confirm($secret, 'forged');

        self::assertResponseStatusCodeSame(403);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email_confirmed_at IS NOT NULL'));
    }

    public function testOnlyTheLatestConfirmationLinkWorks(): void
    {
        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $first = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');

        $crawler = $this->client->request('GET', '/confirm-email/resend');
        $this->client->submit($crawler->selectButton('Send link')->form(['email' => self::EMAIL]));
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
        $this->me($this->signInDevice(self::EMAIL)['access_token']);
        self::assertSame('Noor', $this->decodedResponse()['displayName'] ?? null, 'The name and the password stay.');
    }

    public function testSigningUpAgainBeforeConfirmingReplacesTheFirstSignUp(): void
    {
        $this->register(self::EMAIL, 'Someone Else', 'the-first-password');
        $first = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');

        $this->register(self::EMAIL, 'Noor', self::PASSWORD);
        $second = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $csrf = $this->confirmCsrf($second);

        $this->confirm($first, $csrf);
        self::assertResponseStatusCodeSame(410, 'The first sign-up\'s link ended.');
        $this->confirm($second, $csrf);
        $this->me($this->signInDevice(self::EMAIL, password: self::PASSWORD)['access_token']);
        self::assertSame('Noor', $this->decodedResponse()['displayName'] ?? null);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization'));
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
        self::assertSame(1, $handler(new PurgeAbandonedSignUps()), 'Only the expired link goes.');
        self::assertSame(2, $this->countRows('SELECT COUNT(*) FROM app_user'));

        self::mockTime('+8 days');
        $handler(new PurgeAbandonedSignUps());

        self::assertSame(0, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'noor@example.test'"));
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization'), 'Its organization went with it.');
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'kept@example.test'"));
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

    private function confirm(string $secret, string $csrf): void
    {
        $this->client->request('POST', '/confirm-email', ['token' => $secret, '_csrf_token' => $csrf]);
    }
}
