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
    ];

    public function testCompleteSettingsMayStart(): void
    {
        self::assertSame([], new ProductionSettings(self::complete(), self::DEVELOPMENT)->problems());
    }

    public function testNothingSetStopsTheStart(): void
    {
        self::assertSame([
            'APP_SECRET is not set.',
            'API_PUBLIC_URL must be an https address as a browser writes it (lower case, no path, trailing slash or :443), such as https://varve.example.com.',
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
        self::assertSame([], new ProductionSettings([...self::complete(), 'CORS_ALLOW_ORIGIN' => ''], self::DEVELOPMENT)->problems());
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

    public function testProblemsNeverContainASecret(): void
    {
        $settings = [...self::complete(), 'APP_SECRET' => 'short-secret', 'DATABASE_URL' => 'postgresql://app:weak-pw@postgres:5432/app'];

        $problems = implode("\n", new ProductionSettings($settings, self::DEVELOPMENT)->problems());

        self::assertStringContainsString('APP_SECRET', $problems);
        self::assertStringContainsString('DATABASE_URL', $problems);
        self::assertStringNotContainsString('short-secret', $problems);
        self::assertStringNotContainsString('weak-pw', $problems);
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
        ];
    }
}
