<?php

declare(strict_types=1);

namespace App\Tests\Unit\Deployment;

use App\Deployment\ProductionSettings;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;

final class ProductionSettingsTest extends TestCase
{
    private const array DEVELOPMENT = [
        'DATABASE_URL' => ['postgresql://app:!ChangeMe!@127.0.0.1:5432/app?serverVersion=18&charset=utf8'],
        'APP_SECRET' => ['', '0f5c3e8a9b7d4c2e1a6f8b9c0d1e2f3a', '$ecretf0rt3st'],
        'API_PUBLIC_URL' => ['http://localhost:8090', 'http://localhost'],
        'OAUTH_PASSPHRASE' => ['bc579f574f0955a219d6fe89a67502eb'],
        'OAUTH_ENCRYPTION_KEY' => ['ae08bc1f5adbb2207606ca69fceab39f'],
        'MAILER_DSN' => ['null://null'],
        'VARVE_MAIL_FROM' => ['Varve <accounts@localhost>', 'Varve <accounts@example.test>'],
        'WEB_APP_URL' => ['', 'http://localhost:5173'],
        'REDIS_URL' => ['redis://localhost:6379'],
    ];

    public function testCompleteSettingsMayStart(): void
    {
        self::assertSame([], new ProductionSettings(self::complete(), self::DEVELOPMENT)->problems());
    }

    public function testNothingSetStopsTheStart(): void
    {
        self::assertSame([
            'APP_ENV must be prod: any other environment shows errors in full to every visitor.',
            'APP_SECRET is not set.',
            'OAUTH_PASSPHRASE is not set.',
            'OAUTH_ENCRYPTION_KEY is not set.',
            'API_PUBLIC_URL must be an https address as a browser writes it (lower case, no path, trailing slash or :443), such as https://varve.example.com.',
            'SYMFONY_TRUSTED_PROXIES is not set: list the addresses of the proxy in front of the API, or private_ranges when only that proxy reaches the API over a private network. Without it every visitor counts as the proxy\'s one address.',
            'VARVE_SYNC_MAX_RECORDS must be a whole number above 0.',
            'VARVE_SYNC_MAX_USER_RECORDS must be a whole number above 0.',
            'VARVE_SYNC_MAX_BYTES must be a whole number above 0.',
            'VARVE_SYNC_MAX_USER_BYTES must be a whole number above 0.',
            'VARVE_SYNC_MAX_REVISIONS must be a whole number above 0.',
            'VARVE_SYNC_MAX_REVISION_BYTES must be a whole number above 0.',
            'MAILER_DSN is not set to your mail server, such as smtp://user:password@smtp.example.com:587; account mail would go nowhere.',
            'VARVE_MAIL_FROM must be the sender of account mail on your own domain, such as "Varve <accounts@varve.example.com>".',
            'REDIS_URL must be the redis:// or rediss:// address of your Redis, which holds sign-in sessions and limits.',
            'DATABASE_URL is not set to your database.',
        ], new ProductionSettings([], self::DEVELOPMENT)->problems());
    }

    public function testOnlyTheProductionEnvironmentMayStart(): void
    {
        foreach (['dev', 'test', 'production', ''] as $environment) {
            self::assertSame(
                ['APP_ENV must be prod: any other environment shows errors in full to every visitor.'],
                new ProductionSettings([...self::complete(), 'APP_ENV' => $environment], self::DEVELOPMENT)->problems(),
                $environment,
            );
        }
    }

    public function testDebugModeStopsTheStart(): void
    {
        foreach (['1', 'true', 'on', 'yes', 'ture', '2'] as $debug) {
            self::assertSame(
                ['APP_DEBUG must be 0 or left out: with it on, an error shows the code, the stack and the failed statement to every visitor.'],
                new ProductionSettings([...self::complete(), 'APP_DEBUG' => $debug], self::DEVELOPMENT)->problems(),
                $debug,
            );
        }
        foreach (['0', 'false', ''] as $debug) {
            self::assertSame([], new ProductionSettings([...self::complete(), 'APP_DEBUG' => $debug], self::DEVELOPMENT)->problems(), $debug);
        }
        $withoutDebug = self::complete();
        unset($withoutDebug['APP_DEBUG']);
        self::assertSame([], new ProductionSettings($withoutDebug, self::DEVELOPMENT)->problems());
    }

    public function testTheProxyInFrontMustBeNamed(): void
    {
        foreach (['', '  '] as $proxies) {
            self::assertSame(
                ['SYMFONY_TRUSTED_PROXIES is not set: list the addresses of the proxy in front of the API, or private_ranges when only that proxy reaches the API over a private network. Without it every visitor counts as the proxy\'s one address.'],
                new ProductionSettings([...self::complete(), 'SYMFONY_TRUSTED_PROXIES' => $proxies], self::DEVELOPMENT)->problems(),
            );
        }
        foreach (['private_ranges', 'PRIVATE_SUBNETS', '10.0.0.5', '10.0.0.0/8, 192.168.1.1', '127.0.0.1,::1', 'fd00::/8', '2001:db8::1/128'] as $proxies) {
            self::assertSame([], new ProductionSettings([...self::complete(), 'SYMFONY_TRUSTED_PROXIES' => $proxies], self::DEVELOPMENT)->problems(), $proxies);
        }
    }

    public function testAProxySettingThatTrustsEveryAddressStopsTheStart(): void
    {
        foreach (['0.0.0.0/0', '::/0', 'REMOTE_ADDR', '10.1.2.3/0'] as $proxy) {
            self::assertSame(
                [\sprintf('SYMFONY_TRUSTED_PROXIES lists "%s", which trusts every address: any visitor could then claim another visitor\'s address. List only your proxy.', $proxy)],
                new ProductionSettings([...self::complete(), 'SYMFONY_TRUSTED_PROXIES' => '10.0.0.5,'.$proxy], self::DEVELOPMENT)->problems(),
                $proxy,
            );
        }
    }

    public function testAProxySettingThatIsNotAnAddressStopsTheStartInsteadOfTrustingNobody(): void
    {
        foreach (['proxy', 'none', '*', '10.0.0.0/33', '10.0.0.0/08', '10.0.0.0/', 'fd00::/129', 'private-ranges', '10.0.0'] as $proxy) {
            self::assertSame(
                [\sprintf('SYMFONY_TRUSTED_PROXIES lists "%s", which is not an IP address, a range such as 10.0.0.0/8, or private_ranges.', $proxy)],
                new ProductionSettings([...self::complete(), 'SYMFONY_TRUSTED_PROXIES' => $proxy], self::DEVELOPMENT)->problems(),
                $proxy,
            );
        }
    }

    public function testAMissingSecretStopsTheStart(): void
    {
        $settings = [...self::complete(), 'APP_SECRET' => '  '];

        self::assertSame(['APP_SECRET is not set.'], new ProductionSettings($settings, self::DEVELOPMENT)->problems());
    }

    public function testDevelopmentAndExampleValuesStopTheStart(): void
    {
        foreach ([self::DEVELOPMENT['APP_SECRET'][1], 'change-me-change-me-change-me-change-me'] as $secret) {
            self::assertSame(
                ['APP_SECRET is still a development or example value; generate a new one.'],
                new ProductionSettings([...self::complete(), 'APP_SECRET' => $secret], self::DEVELOPMENT)->problems(),
            );
        }
    }

    public function testShortSecretsStopTheStart(): void
    {
        self::assertSame(
            ['APP_SECRET must be at least 32 characters.'],
            new ProductionSettings([...self::complete(), 'APP_SECRET' => 'too-short'], self::DEVELOPMENT)->problems(),
        );
    }

    public function testTheExampleDatabaseStopsTheStart(): void
    {
        self::assertSame(
            ['DATABASE_URL is not set to your database.'],
            new ProductionSettings([...self::complete(), 'DATABASE_URL' => self::DEVELOPMENT['DATABASE_URL'][0]], self::DEVELOPMENT)->problems(),
        );
    }

    public function testAWeakDatabasePasswordStopsTheStart(): void
    {
        $settings = [...self::complete(), 'DATABASE_URL' => 'postgresql://app:app@postgres:5432/app?serverVersion=18'];

        self::assertSame(
            ['The database password in DATABASE_URL must be at least 16 characters.'],
            new ProductionSettings($settings, self::DEVELOPMENT)->problems(),
        );
    }

    /** @return iterable<string, array{string}> */
    public static function addressesThatAreNotAnOrigin(): iterable
    {
        yield 'development address' => ['http://localhost:8090'];
        yield 'trailing slash' => ['https://varve.example.com/'];
        yield 'path' => ['https://varve.example.com/varve'];
        yield 'plain http' => ['http://varve.example.com'];
        yield 'no scheme' => ['varve.example.com'];
        yield 'default port written out' => ['https://varve.example.com:443'];
        yield 'leading zero in the port' => ['https://varve.example.com:08443'];
        yield 'port above 65535' => ['https://varve.example.com:70000'];
        yield 'upper case' => ['https://Varve.Example.com'];
        yield 'query' => ['https://varve.example.com?x=1'];
        yield 'user' => ['https://user@varve.example.com'];
    }

    #[DataProvider('addressesThatAreNotAnOrigin')]
    public function testTheApiAddressMustBeABareHttpsOrigin(string $address): void
    {
        self::assertSame(
            ['API_PUBLIC_URL must be an https address as a browser writes it (lower case, no path, trailing slash or :443), such as https://varve.example.com.'],
            new ProductionSettings([...self::complete(), 'API_PUBLIC_URL' => $address], self::DEVELOPMENT)->problems(),
        );
    }

    public function testTheApiAddressMayHaveAPort(): void
    {
        self::assertSame([], new ProductionSettings([...self::complete(), 'API_PUBLIC_URL' => 'https://api.varve.example.com:8443'], self::DEVELOPMENT)->problems());
    }

    public function testCorsIsAListOfHttpsOrigins(): void
    {
        self::assertSame(
            ['CORS_ALLOW_ORIGIN must list origins separated by spaces, not a regular expression.'],
            new ProductionSettings([...self::complete(), 'CORS_ALLOW_ORIGIN' => '^https://varve\\.example\\.com$'], self::DEVELOPMENT)->problems(),
        );
        self::assertSame(
            [
                'CORS_ALLOW_ORIGIN lists "https://*.example.com", which is not an https origin as a browser writes it, such as https://varve.example.com.',
                'CORS_ALLOW_ORIGIN lists "http://intranet.example", which is not an https origin as a browser writes it, such as https://varve.example.com.',
            ],
            new ProductionSettings([...self::complete(), 'CORS_ALLOW_ORIGIN' => 'https://varve.example.com https://*.example.com http://intranet.example'], self::DEVELOPMENT)->problems(),
        );
    }

    public function testCorsMayAllowNoOriginAtAll(): void
    {
        self::assertSame([], new ProductionSettings([...self::complete(), 'CORS_ALLOW_ORIGIN' => '', 'WEB_APP_URL' => ''], self::DEVELOPMENT)->problems());
    }

    /** @return iterable<string, array{string}> */
    public static function billingSwitches(): iterable
    {
        yield 'off' => ['0'];
        yield 'on' => ['1'];
        yield 'empty' => [''];
        yield 'words' => ['true'];
    }

    #[DataProvider('billingSwitches')]
    public function testBillingIsOnOrOff(string $value): void
    {
        self::assertSame([], new ProductionSettings([...self::complete(), 'VARVE_BILLING' => $value], self::DEVELOPMENT)->problems());
    }

    #[DataProvider('billingSwitches')]
    public function testSignUpIsOpenOrClosed(string $value): void
    {
        self::assertSame([], new ProductionSettings([...self::complete(), 'VARVE_SIGN_UP' => $value], self::DEVELOPMENT)->problems());
    }

    public function testAMisspelledSignUpSwitchStopsTheStartInsteadOfReadingAsClosed(): void
    {
        foreach (['ture', '2', 'open', 'closed'] as $value) {
            self::assertSame(
                ['VARVE_SIGN_UP must be 0 or 1.'],
                new ProductionSettings([...self::complete(), 'VARVE_SIGN_UP' => $value], self::DEVELOPMENT)->problems(),
                $value,
            );
        }
    }

    public function testAMisspelledBillingSwitchStopsTheStartInsteadOfReadingAsOff(): void
    {
        foreach (['ture', '2', 'enabled'] as $value) {
            self::assertSame(
                ['VARVE_BILLING must be 0 or 1.'],
                new ProductionSettings([...self::complete(), 'VARVE_BILLING' => $value], self::DEVELOPMENT)->problems(),
                $value,
            );
        }
    }

    public function testRecordLimitsAreWholeNumbersAboveZero(): void
    {
        foreach (['', '0', '-5', '1.5', 'many', '10000000000'] as $value) {
            self::assertSame(
                ['VARVE_SYNC_MAX_RECORDS must be a whole number above 0.'],
                new ProductionSettings([...self::complete(), 'VARVE_SYNC_MAX_RECORDS' => $value], self::DEVELOPMENT)->problems(),
                $value,
            );
        }
        self::assertSame([], new ProductionSettings([...self::complete(), 'VARVE_SYNC_MAX_USER_RECORDS' => '1'], self::DEVELOPMENT)->problems());
    }

    /** @return iterable<string, array{string, string, string}> */
    public static function syncLimits(): iterable
    {
        // The setting, the largest value it takes, and the smallest one that is too long.
        yield 'records of an organization' => ['VARVE_SYNC_MAX_RECORDS', '999999999', '1000000000'];
        yield 'records of a user' => ['VARVE_SYNC_MAX_USER_RECORDS', '999999999', '1000000000'];
        yield 'bytes of an organization' => ['VARVE_SYNC_MAX_BYTES', '999999999999', '1000000000000'];
        yield 'bytes of a user' => ['VARVE_SYNC_MAX_USER_BYTES', '999999999999', '1000000000000'];
        yield 'earlier versions of a saved query' => ['VARVE_SYNC_MAX_REVISIONS', '999999999', '1000000000'];
        yield 'bytes of those versions' => ['VARVE_SYNC_MAX_REVISION_BYTES', '999999999', '1000000000'];
    }

    #[DataProvider('syncLimits')]
    public function testEverySyncLimitIsAWholeNumberAboveZero(string $name, string $largest, string $tooLong): void
    {
        foreach (['', '0', '-5', '1.5', 'many', '8M', '050', $tooLong] as $value) {
            self::assertSame(
                [$name.' must be a whole number above 0.'],
                new ProductionSettings([...self::complete(), $name => $value], self::DEVELOPMENT)->problems(),
                $value,
            );
        }
        foreach (['1', $largest] as $value) {
            self::assertSame([], new ProductionSettings([...self::complete(), $name => $value], self::DEVELOPMENT)->problems(), $value);
        }
    }

    public function testTheByteLimitsTakeMoreThanNineDigits(): void
    {
        // 16 GiB for an organization and 4 GiB for a user: past the nine digits a count stops at.
        $settings = [...self::complete(), 'VARVE_SYNC_MAX_BYTES' => '17179869184', 'VARVE_SYNC_MAX_USER_BYTES' => '4294967296'];

        self::assertSame([], new ProductionSettings($settings, self::DEVELOPMENT)->problems());
    }

    public function testTheOAuthKeysMustBeSetLongAndNotTheDevelopmentOnes(): void
    {
        self::assertSame(
            ['OAUTH_PASSPHRASE is still a development or example value; generate a new one.', 'OAUTH_ENCRYPTION_KEY is still a development or example value; generate a new one.'],
            new ProductionSettings([...self::complete(), 'OAUTH_PASSPHRASE' => self::DEVELOPMENT['OAUTH_PASSPHRASE'][0], 'OAUTH_ENCRYPTION_KEY' => self::DEVELOPMENT['OAUTH_ENCRYPTION_KEY'][0]], self::DEVELOPMENT)->problems(),
        );
        self::assertSame(
            ['OAUTH_PASSPHRASE must be at least 16 characters.', 'OAUTH_ENCRYPTION_KEY must be at least 32 characters.'],
            new ProductionSettings([...self::complete(), 'OAUTH_PASSPHRASE' => 'short', 'OAUTH_ENCRYPTION_KEY' => str_repeat('k', 31)], self::DEVELOPMENT)->problems(),
        );
    }

    public function testAccountMailMustGoOut(): void
    {
        foreach (['', 'null://null', 'null://default'] as $dsn) {
            self::assertSame(
                ['MAILER_DSN is not set to your mail server, such as smtp://user:password@smtp.example.com:587; account mail would go nowhere.'],
                new ProductionSettings([...self::complete(), 'MAILER_DSN' => $dsn], self::DEVELOPMENT)->problems(),
                $dsn,
            );
        }
    }

    public function testAccountMailComesFromTheServersOwnDomain(): void
    {
        foreach (['', 'Varve <accounts@localhost>', 'accounts@example.test', 'not an address', 'Varve <accounts@varve.invalid>'] as $from) {
            self::assertSame(
                ['VARVE_MAIL_FROM must be the sender of account mail on your own domain, such as "Varve <accounts@varve.example.com>".'],
                new ProductionSettings([...self::complete(), 'VARVE_MAIL_FROM' => $from], self::DEVELOPMENT)->problems(),
                $from,
            );
        }
        self::assertSame([], new ProductionSettings([...self::complete(), 'VARVE_MAIL_FROM' => 'accounts@varve.example.com'], self::DEVELOPMENT)->problems());
    }

    public function testRedisMustBeARedisAddress(): void
    {
        foreach (['', 'localhost:6379', 'http://redis:6379'] as $url) {
            self::assertSame(
                ['REDIS_URL must be the redis:// or rediss:// address of your Redis, which holds sign-in sessions and limits.'],
                new ProductionSettings([...self::complete(), 'REDIS_URL' => $url], self::DEVELOPMENT)->problems(),
                $url,
            );
        }
        self::assertSame([], new ProductionSettings([...self::complete(), 'REDIS_URL' => 'rediss://:secret@redis.internal:6380'], self::DEVELOPMENT)->problems());
    }

    public function testRedisIsNeverTheApiContainerItself(): void
    {
        $local = [self::DEVELOPMENT['REDIS_URL'][0], 'redis://localhost', 'redis://LOCALHOST:6380/1', 'redis://:secret@127.0.0.1:6379', 'rediss://[::1]:6379', 'redis://redis.localhost:6379', 'redis://0.0.0.0:6379'];
        foreach ($local as $url) {
            self::assertSame(
                ['REDIS_URL is not set to your Redis: localhost is the API container itself, which runs none.'],
                new ProductionSettings([...self::complete(), 'REDIS_URL' => $url], self::DEVELOPMENT)->problems(),
                $url,
            );
        }
        // A socket has no host, and a name that only starts like localhost is another machine.
        foreach (['redis:///var/run/redis/redis.sock', 'redis://localhost.example.com:6379'] as $url) {
            self::assertSame([], new ProductionSettings([...self::complete(), 'REDIS_URL' => $url], self::DEVELOPMENT)->problems(), $url);
        }
    }

    public function testAServerWithoutAWebClientLeavesItsAddressEmpty(): void
    {
        self::assertSame([], new ProductionSettings([...self::complete(), 'WEB_APP_URL' => ''], self::DEVELOPMENT)->problems());
        self::assertSame(
            ['WEB_APP_URL must be empty or an https address as a browser writes it (lower case, no path, trailing slash or :443), such as https://varve.example.com.'],
            new ProductionSettings([...self::complete(), 'WEB_APP_URL' => 'http://localhost:5173'], self::DEVELOPMENT)->problems(),
        );
    }

    public function testCorsMustListTheWebClient(): void
    {
        self::assertSame(
            ['CORS_ALLOW_ORIGIN does not list WEB_APP_URL, so the web client cannot call the API.'],
            new ProductionSettings([...self::complete(), 'WEB_APP_URL' => 'https://app.varve.example.com'], self::DEVELOPMENT)->problems(),
        );
    }

    public function testProblemsNeverContainASecret(): void
    {
        $settings = [...self::complete(), 'APP_SECRET' => 'short-secret', 'DATABASE_URL' => 'postgresql://app:weak-pw@postgres:5432/app', 'OAUTH_PASSPHRASE' => 'short-pass'];

        $problems = implode("\n", new ProductionSettings($settings, self::DEVELOPMENT)->problems());

        self::assertStringContainsString('APP_SECRET', $problems);
        self::assertStringContainsString('DATABASE_URL', $problems);
        self::assertStringNotContainsString('short-secret', $problems);
        self::assertStringNotContainsString('weak-pw', $problems);
        self::assertStringNotContainsString('short-pass', $problems);
    }

    /** @return array<string, string> */
    private static function complete(): array
    {
        return [
            'APP_ENV' => 'prod',
            'APP_DEBUG' => '0',
            'APP_SECRET' => str_repeat('a', 32),
            'DATABASE_URL' => 'postgresql://varve:'.str_repeat('p', 32).'@postgres:5432/varve?serverVersion=18&charset=utf8',
            'API_PUBLIC_URL' => 'https://api.varve.example.com',
            'CORS_ALLOW_ORIGIN' => 'https://varve.example.com tauri://localhost http://tauri.localhost',
            'SYMFONY_TRUSTED_PROXIES' => 'private_ranges',
            'VARVE_BILLING' => '0',
            'VARVE_SIGN_UP' => '1',
            'VARVE_SYNC_MAX_RECORDS' => '50000',
            'VARVE_SYNC_MAX_USER_RECORDS' => '10000',
            'VARVE_SYNC_MAX_BYTES' => '536870912',
            'VARVE_SYNC_MAX_USER_BYTES' => '67108864',
            'VARVE_SYNC_MAX_REVISIONS' => '50',
            'VARVE_SYNC_MAX_REVISION_BYTES' => '8388608',
            'OAUTH_PASSPHRASE' => str_repeat('p', 32),
            'OAUTH_ENCRYPTION_KEY' => str_repeat('e', 32),
            'MAILER_DSN' => 'smtp://varve:'.str_repeat('m', 16).'@smtp.example.com:587',
            'VARVE_MAIL_FROM' => 'Varve <accounts@varve.example.com>',
            'WEB_APP_URL' => 'https://varve.example.com',
            'REDIS_URL' => 'redis://redis:6379',
        ];
    }
}
