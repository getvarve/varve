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
    ];

    public function testCompleteSettingsMayStart(): void
    {
        self::assertSame([], new ProductionSettings(self::complete(), self::DEVELOPMENT)->problems());
    }

    public function testNothingSetStopsTheStart(): void
    {
        self::assertSame([
            'APP_SECRET is not set.',
            'OAUTH_PASSPHRASE is not set.',
            'OAUTH_ENCRYPTION_KEY is not set.',
            'API_PUBLIC_URL must be an https address as a browser writes it (lower case, no path, trailing slash or :443), such as https://varve.example.com.',
            'VARVE_SYNC_MAX_RECORDS must be a whole number above 0.',
            'VARVE_SYNC_MAX_USER_RECORDS must be a whole number above 0.',
            'MAILER_DSN is not set to your mail server, such as smtp://user:password@smtp.example.com:587; account mail would go nowhere.',
            'VARVE_MAIL_FROM must be the sender of account mail on your own domain, such as "Varve <accounts@varve.example.com>".',
            'REDIS_URL must be the redis:// or rediss:// address of your Redis, which holds sign-in sessions and limits.',
            'DATABASE_URL is not set to your database.',
        ], new ProductionSettings([], self::DEVELOPMENT)->problems());
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
            'APP_SECRET' => str_repeat('a', 32),
            'DATABASE_URL' => 'postgresql://varve:'.str_repeat('p', 32).'@postgres:5432/varve?serverVersion=18&charset=utf8',
            'API_PUBLIC_URL' => 'https://api.varve.example.com',
            'CORS_ALLOW_ORIGIN' => 'https://varve.example.com tauri://localhost http://tauri.localhost',
            'VARVE_BILLING' => '0',
            'VARVE_SYNC_MAX_RECORDS' => '50000',
            'VARVE_SYNC_MAX_USER_RECORDS' => '10000',
            'OAUTH_PASSPHRASE' => str_repeat('p', 32),
            'OAUTH_ENCRYPTION_KEY' => str_repeat('e', 32),
            'MAILER_DSN' => 'smtp://varve:'.str_repeat('m', 16).'@smtp.example.com:587',
            'VARVE_MAIL_FROM' => 'Varve <accounts@varve.example.com>',
            'WEB_APP_URL' => 'https://varve.example.com',
            'REDIS_URL' => 'redis://redis:6379',
        ];
    }
}
