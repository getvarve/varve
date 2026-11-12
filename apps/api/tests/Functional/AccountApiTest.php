<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Doctrine\DBAL\Exception\DriverException;

/**
 * The signed-in user's account on the API: who they are, their organizations, a new password and deleting the account.
 */
final class AccountApiTest extends DatabaseTestCase
{
    use AuthenticatesUsers;

    private const string KIM = 'kim@example.test';
    private const string NEW_PASSWORD = 'a-brand-new-password';

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->createUser(self::KIM, 'Kim Visser');
    }

    public function testMeIsTheUserWithTheirPersonalOrganization(): void
    {
        $this->me($this->signInDevice(self::KIM)['access_token']);

        self::assertResponseIsSuccessful();
        $me = $this->decodedResponse();
        self::assertSame(['id', 'email', 'displayName', 'createdAt', 'personalOrganization'], array_keys($me));
        self::assertSame(self::KIM, $me['email']);
        self::assertSame('Kim Visser', $me['displayName']);
        $organization = $me['personalOrganization'];
        self::assertIsArray($organization);
        self::assertSame('Kim Visser', $organization['name'] ?? null, 'A personal organization goes by its owner\'s name.');
        self::assertSame(1, $organization['seats'] ?? null);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization WHERE id = :id AND personal_owner_id = :user', ['id' => $organization['id'] ?? null, 'user' => $me['id']]));
    }

    public function testTheOrganizationsAreTheUsersOwn(): void
    {
        $this->createUser('lisa@example.test', 'Lisa Bakker');

        $this->client->request('GET', '/api/v1/organizations', server: $this->userHeaders(self::KIM));

        self::assertResponseIsSuccessful();
        $organizations = $this->decodedResponse();
        self::assertCount(1, $organizations);
        $organization = $organizations[0];
        self::assertIsArray($organization);
        self::assertSame('Kim Visser', $organization['name'] ?? null);
        self::assertTrue($organization['personal'] ?? null);
        self::assertSame('owner', $organization['role'] ?? null);
        self::assertSame(1, $organization['seats'] ?? null);
    }

    public function testAPersonalOrganizationIsNamedOrOwnedNeverBoth(): void
    {
        $this->expectException(DriverException::class);
        $this->expectExceptionMessage('organization_team_or_personal');

        $this->connection()->executeStatement("UPDATE organization SET name = 'A team'");
    }

    public function testChangingThePasswordSignsOutEveryOtherDevice(): void
    {
        $laptop = $this->signInDevice(self::KIM, 'MacBook Pro');
        $other = $this->signInDevice(self::KIM, 'Work PC');

        $this->changePassword($laptop['access_token'], self::DEFAULT_PASSWORD, self::NEW_PASSWORD);

        self::assertResponseStatusCodeSame(204);
        $this->me($laptop['access_token']);
        self::assertResponseIsSuccessful('The device that changed it stays signed in.');
        $this->refresh($laptop['refresh_token']);
        self::assertResponseIsSuccessful();
        $this->me($other['access_token']);
        self::assertResponseStatusCodeSame(401, 'Every other device is out.');
        $this->refresh($other['refresh_token']);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);

        $this->signInDevice(self::KIM, password: self::NEW_PASSWORD);
    }

    public function testAPasswordChangeNeedsTheCurrentPassword(): void
    {
        $tokens = $this->signInDevice(self::KIM);

        $this->changePassword($tokens['access_token'], 'not-the-password', self::NEW_PASSWORD);

        self::assertResponseStatusCodeSame(422);
        self::assertSame('currentPassword', $this->violations()[0]['propertyPath'] ?? null);
        $this->signInDevice(self::KIM);
    }

    public function testANewPasswordMustBeLongEnough(): void
    {
        $tokens = $this->signInDevice(self::KIM);

        $this->changePassword($tokens['access_token'], self::DEFAULT_PASSWORD, 'too-short');

        self::assertResponseStatusCodeSame(422);
        self::assertSame('newPassword', $this->violations()[0]['propertyPath'] ?? null);
    }

    public function testPasswordChecksAreLimited(): void
    {
        $tokens = $this->signInDevice(self::KIM);
        for ($check = 1; $check <= 5; ++$check) {
            $this->changePassword($tokens['access_token'], 'guess-'.$check, self::NEW_PASSWORD);
            self::assertResponseStatusCodeSame(422, 'Check '.$check);
        }

        $this->changePassword($tokens['access_token'], self::DEFAULT_PASSWORD, self::NEW_PASSWORD);

        self::assertResponseStatusCodeSame(429, 'Even the right password waits.');
        self::assertTrue($this->client->getResponse()->headers->has('Retry-After'));
        $this->client->request('POST', '/api/v1/me/deletion', server: [...self::bearer($tokens['access_token']), 'CONTENT_TYPE' => 'application/json'], content: json_encode(['password' => self::DEFAULT_PASSWORD], \JSON_THROW_ON_ERROR));
        self::assertResponseStatusCodeSame(429, 'Deleting the account checks the password under the same limit.');
    }

    public function testDeletingTheAccountRemovesEverythingAndEndsEveryToken(): void
    {
        $this->createUser('lisa@example.test', 'Lisa Bakker');
        $lisa = $this->signInDevice('lisa@example.test');
        $laptop = $this->signInDevice(self::KIM);
        $other = $this->signInDevice(self::KIM, 'Work PC');

        $this->deleteAccount($laptop['access_token'], self::DEFAULT_PASSWORD);

        self::assertResponseStatusCodeSame(204);
        self::assertSame(0, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'kim@example.test'"));
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization'), 'Lisa\'s organization stays.');
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM device'));
        self::assertSame(0, $this->countRows("SELECT COUNT(*) FROM oauth2_access_token WHERE user_identifier = 'kim@example.test'"));
        foreach ([$laptop, $other] as $tokens) {
            $this->me($tokens['access_token']);
            self::assertResponseStatusCodeSame(401);
            $this->refresh($tokens['refresh_token']);
            self::assertResponseStatusCodeSame(400);
        }
        $this->me($lisa['access_token']);
        self::assertResponseIsSuccessful();
    }

    public function testDeletingTheAccountNeedsThePassword(): void
    {
        $tokens = $this->signInDevice(self::KIM);

        $this->deleteAccount($tokens['access_token'], 'not-the-password');

        self::assertResponseStatusCodeSame(422);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    public function testAccountChangesNeedTheAccountScope(): void
    {
        $verifier = bin2hex(random_bytes(32));
        $crawler = $this->client->request('GET', '/login');
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => self::KIM, '_password' => self::DEFAULT_PASSWORD]));
        $this->client->request('GET', $this->desktopAuthorizeUrl($verifier, ['scope' => 'sync']));
        $this->client->request('POST', '/oauth/token', $this->codeExchange($this->codeFrom((string) $this->client->getResponse()->headers->get('Location')), $verifier));
        $token = self::string($this->decodedResponse()['access_token'] ?? null);

        $this->deleteAccount($token, self::DEFAULT_PASSWORD);
        self::assertResponseStatusCodeSame(403);
        $this->changePassword($token, self::DEFAULT_PASSWORD, self::NEW_PASSWORD);
        self::assertResponseStatusCodeSame(403);
        $this->client->request('GET', '/api/v1/devices', server: self::bearer($token));
        self::assertResponseStatusCodeSame(403);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    private function changePassword(string $accessToken, string $current, string $new): void
    {
        $this->client->request('POST', '/api/v1/me/password', server: [...self::bearer($accessToken), 'CONTENT_TYPE' => 'application/json'], content: json_encode(['currentPassword' => $current, 'newPassword' => $new], \JSON_THROW_ON_ERROR));
    }

    private function deleteAccount(string $accessToken, string $password): void
    {
        $this->client->request('POST', '/api/v1/me/deletion', server: [...self::bearer($accessToken), 'CONTENT_TYPE' => 'application/json'], content: json_encode(['password' => $password], \JSON_THROW_ON_ERROR));
    }

    /** @return list<array<mixed>> */
    private function violations(): array
    {
        $violations = $this->decodedResponse()['violations'] ?? null;
        self::assertIsArray($violations);

        return array_values(array_filter($violations, \is_array(...)));
    }
}
