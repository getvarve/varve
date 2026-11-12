<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Security\OAuth\PurgeExpiredOAuthTokens;
use App\Security\OAuth\PurgeExpiredOAuthTokensHandler;
use Symfony\Component\Clock\Test\ClockSensitiveTrait;

/**
 * Every sign-in belongs to a device; the user lists, renames and signs out their devices, and only theirs.
 */
final class DeviceApiTest extends DatabaseTestCase
{
    use AuthenticatesUsers;
    use ClockSensitiveTrait;

    private const string KIM = 'kim@example.test';
    private const string LISA = 'lisa@example.test';

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->createUser(self::KIM, 'Kim Visser');
        $this->createUser(self::LISA, 'Lisa Bakker');
    }

    public function testASignInStartsADeviceThatTheUserSees(): void
    {
        $laptop = $this->signInDevice(self::KIM, 'MacBook Pro');
        $this->signInDevice(self::KIM, 'Work PC');
        $this->signInDevice(self::LISA, 'Lisa\'s laptop');

        $this->client->request('GET', '/api/v1/devices', server: self::bearer($laptop['access_token']));

        self::assertResponseIsSuccessful();
        $devices = $this->decodedResponse();
        self::assertCount(2, $devices, 'Only Kim\'s own devices.');
        $byName = array_column($devices, null, 'name');
        self::assertEqualsCanonicalizing(['MacBook Pro', 'Work PC'], array_keys($byName));
        $mac = $byName['MacBook Pro'];
        self::assertIsArray($mac);
        self::assertTrue($mac['current'] ?? null, 'The device the request comes from.');
        self::assertSame('macos', $mac['platform'] ?? null);
        self::assertSame('desktop', $mac['app'] ?? null);
        self::assertSame('0.4.0', $mac['appVersion'] ?? null);
        self::assertIsString($mac['createdAt'] ?? null);
        self::assertIsString($mac['lastSeenAt'] ?? null);
        $work = $byName['Work PC'];
        self::assertIsArray($work);
        self::assertFalse($work['current'] ?? null);
    }

    public function testACodeExchangeMustNameTheDevice(): void
    {
        foreach ([
            'no name' => ['device_name' => ''],
            'control characters' => ['device_name' => "Mac\u{202E}Book"],
            'too long' => ['device_name' => str_repeat('x', 101)],
            'unknown platform' => ['device_platform' => 'amiga'],
            'no version' => ['app_version' => ''],
            'odd version' => ['app_version' => '1.0 beta'],
        ] as $case => $override) {
            $verifier = bin2hex(random_bytes(32));
            $this->signInBrowser(self::KIM);
            $this->client->request('GET', $this->desktopAuthorizeUrl($verifier));
            $code = $this->codeFrom((string) $this->client->getResponse()->headers->get('Location'));

            $this->client->request('POST', '/oauth/token', [...$this->codeExchange($code, $verifier), ...$override]);

            self::assertResponseStatusCodeSame(400, $case);
            self::assertSame('invalid_request', $this->decodedResponse()['error'] ?? null, $case);

            $this->client->request('POST', '/oauth/token', $this->codeExchange($code, $verifier));
            self::assertResponseIsSuccessful($case.': the code was not spent.');
        }
        self::assertSame(\count([1, 2, 3, 4, 5, 6]), $this->countRows('SELECT COUNT(*) FROM device'));
    }

    public function testARefreshKeepsTheDeviceAndTakesTheNewAppVersion(): void
    {
        $tokens = $this->signInDevice(self::KIM);

        self::mockTime('+10 minutes');
        $this->refresh($tokens['refresh_token'], ['app_version' => '0.5.0']);
        self::assertResponseIsSuccessful();
        $rotated = $this->decodedResponse();

        $this->client->request('GET', '/api/v1/devices', server: self::bearer(self::string($rotated['access_token'] ?? null)));
        $devices = $this->decodedResponse();
        self::assertCount(1, $devices);
        $device = $devices[0];
        self::assertIsArray($device);
        self::assertSame('0.5.0', $device['appVersion'] ?? null);
        self::assertTrue($device['current'] ?? null, 'The new token belongs to the same device.');
        self::assertNotSame($device['createdAt'] ?? null, $device['lastSeenAt'] ?? null, 'The refresh marked it seen.');
    }

    public function testAnOddAppVersionOnARefreshIsRefused(): void
    {
        $tokens = $this->signInDevice(self::KIM);

        $this->refresh($tokens['refresh_token'], ['app_version' => 'not a version']);

        self::assertResponseStatusCodeSame(400);
        $this->refresh($tokens['refresh_token']);
        self::assertResponseIsSuccessful('The refresh token was not spent.');
    }

    public function testRenamingADevice(): void
    {
        $tokens = $this->signInDevice(self::KIM);
        $id = $this->deviceIds($tokens['access_token'])[0];

        $this->rename($tokens['access_token'], $id, '  Kim\'s MacBook  ');

        self::assertResponseIsSuccessful();
        self::assertSame('Kim\'s MacBook', $this->decodedResponse()['name'] ?? null);
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM device WHERE name = 'Kim''s MacBook'"));
    }

    public function testANameMustBeANameOfAtMostAHundredCharacters(): void
    {
        $tokens = $this->signInDevice(self::KIM);
        $id = $this->deviceIds($tokens['access_token'])[0];

        foreach (['   ', str_repeat('x', 101), "Line\nbreak"] as $name) {
            $this->rename($tokens['access_token'], $id, $name);
            self::assertResponseStatusCodeSame(422, json_encode($name, \JSON_THROW_ON_ERROR));
        }
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM device WHERE name = 'MacBook Pro'"));
    }

    public function testSigningOutADeviceEndsItsTokensAtOnce(): void
    {
        $laptop = $this->signInDevice(self::KIM, 'MacBook Pro');
        $other = $this->signInDevice(self::KIM, 'Work PC');
        $workPc = $this->currentDeviceId($other['access_token']);

        $this->client->request('DELETE', '/api/v1/devices/'.$workPc, server: self::bearer($laptop['access_token']));

        self::assertResponseStatusCodeSame(204);
        $this->me($other['access_token']);
        self::assertResponseStatusCodeSame(401, 'The signed-out device\'s access token stops working.');
        $this->refresh($other['refresh_token']);
        self::assertResponseStatusCodeSame(400);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null);
        self::assertSame(['MacBook Pro'], array_column($this->devices($laptop['access_token']), 'name'));
    }

    public function testSigningOutTheCurrentDevice(): void
    {
        $tokens = $this->signInDevice(self::KIM);

        $this->client->request('DELETE', '/api/v1/devices/'.$this->currentDeviceId($tokens['access_token']), server: self::bearer($tokens['access_token']));

        self::assertResponseStatusCodeSame(204);
        $this->me($tokens['access_token']);
        self::assertResponseStatusCodeSame(401);
    }

    public function testATokenOfASignedOutDeviceDoesNotWorkEvenWithinTheRefreshGrace(): void
    {
        $tokens = $this->signInDevice(self::KIM);
        $this->refresh($tokens['refresh_token']);
        $rotated = $this->decodedResponse();
        $this->me($tokens['access_token']);
        self::assertResponseIsSuccessful('Within the grace the old access token still works.');

        $this->client->request('DELETE', '/api/v1/devices/'.$this->currentDeviceId(self::string($rotated['access_token'] ?? null)), server: self::bearer(self::string($rotated['access_token'] ?? null)));

        $this->me($tokens['access_token']);
        self::assertResponseStatusCodeSame(401);
        $this->refresh($tokens['refresh_token']);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null, 'Nor does the grace answer again.');
    }

    public function testAnotherUsersDeviceIsNotThere(): void
    {
        $kim = $this->signInDevice(self::KIM);
        $lisa = $this->signInDevice(self::LISA);
        $lisasDevice = $this->currentDeviceId($lisa['access_token']);

        $this->rename($kim['access_token'], $lisasDevice, 'Mine now');
        self::assertResponseStatusCodeSame(404);
        $this->client->request('DELETE', '/api/v1/devices/'.$lisasDevice, server: self::bearer($kim['access_token']));
        self::assertResponseStatusCodeSame(404);

        $this->me($lisa['access_token']);
        self::assertResponseIsSuccessful('Lisa stays signed in.');
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM device WHERE name = 'MacBook Pro' AND id = :id", ['id' => $lisasDevice]));
    }

    public function testAnUnknownDeviceIsNotThere(): void
    {
        $kim = $this->signInDevice(self::KIM);

        foreach (['01900000-0000-7000-8000-000000000000', 'not-a-uuid'] as $id) {
            $this->client->request('DELETE', '/api/v1/devices/'.$id, server: self::bearer($kim['access_token']));
            self::assertResponseStatusCodeSame(404, $id);
        }
    }

    public function testATokenWithoutADeviceDoesNotWork(): void
    {
        $tokens = $this->signInDevice(self::KIM);
        $this->connection()->executeStatement('DELETE FROM device_session');

        $this->me($tokens['access_token']);

        self::assertResponseStatusCodeSame(401);
        $this->refresh($tokens['refresh_token']);
        self::assertSame('invalid_grant', $this->decodedResponse()['error'] ?? null, 'A refresh without its device is refused, and what it issued revoked.');
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM oauth2_refresh_token WHERE revoked = false'));
    }

    public function testRequestsMarkTheDeviceSeenAtMostEveryFiveMinutes(): void
    {
        $tokens = $this->signInDevice(self::KIM);
        $seen = fn (): string => self::string($this->connection()->fetchOne('SELECT last_seen_at FROM device'));
        $signedIn = $seen();

        self::mockTime('+2 minutes');
        $this->me($tokens['access_token']);
        self::assertSame($signedIn, $seen());

        self::mockTime('+6 minutes');
        $this->me($tokens['access_token']);
        self::assertNotSame($signedIn, $seen());
    }

    public function testADeviceWhoseTokensExpiredLeavesTheListAndThePurgeRemovesIt(): void
    {
        $tokens = $this->signInDevice(self::KIM);
        $other = $this->signInDevice(self::KIM, 'Work PC');
        $this->connection()->executeStatement(
            "UPDATE oauth2_refresh_token SET expiry = NOW() - INTERVAL '1 day' WHERE access_token IN (SELECT s.access_token FROM device_session s JOIN device d ON d.id = s.device_id WHERE d.name = 'Work PC')",
        );
        $this->connection()->executeStatement(
            "UPDATE oauth2_access_token SET expiry = NOW() - INTERVAL '1 day' WHERE identifier IN (SELECT s.access_token FROM device_session s JOIN device d ON d.id = s.device_id WHERE d.name = 'Work PC')",
        );

        self::assertSame(['MacBook Pro'], array_column($this->devices($tokens['access_token']), 'name'));

        $handler = static::getContainer()->get(PurgeExpiredOAuthTokensHandler::class);
        self::assertSame(3, $handler(new PurgeExpiredOAuthTokens()), 'Its refresh token, its access token and the device.');
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM device'));
        $this->me($other['access_token']);
        self::assertResponseStatusCodeSame(401);
    }

    /** @return list<array<mixed>> */
    private function devices(string $accessToken): array
    {
        $this->client->request('GET', '/api/v1/devices', server: self::bearer($accessToken));
        self::assertResponseIsSuccessful();

        return array_values(array_filter($this->decodedResponse(), \is_array(...)));
    }

    /** @return list<string> */
    private function deviceIds(string $accessToken): array
    {
        return array_map(static fn (array $device): string => self::string($device['id'] ?? null), $this->devices($accessToken));
    }

    private function currentDeviceId(string $accessToken): string
    {
        foreach ($this->devices($accessToken) as $device) {
            if (true === ($device['current'] ?? null)) {
                return self::string($device['id'] ?? null);
            }
        }
        self::fail('No current device.');
    }

    private function rename(string $accessToken, string $id, string $name): void
    {
        $this->client->request('PATCH', '/api/v1/devices/'.$id, server: [...self::bearer($accessToken), 'CONTENT_TYPE' => 'application/merge-patch+json'], content: json_encode(['name' => $name], \JSON_THROW_ON_ERROR));
    }

    private function signInBrowser(string $email): void
    {
        $this->client->getCookieJar()->clear();
        $crawler = $this->client->request('GET', '/login');
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => $email, '_password' => self::DEFAULT_PASSWORD]));
    }
}
