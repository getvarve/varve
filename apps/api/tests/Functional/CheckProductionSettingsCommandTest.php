<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Symfony\Bundle\FrameworkBundle\Console\Application;
use Symfony\Bundle\FrameworkBundle\Test\KernelTestCase;
use Symfony\Component\Console\Tester\CommandTester;

final class CheckProductionSettingsCommandTest extends KernelTestCase
{
    /** @var array<string, array{mixed, mixed}> */
    private array $previous = [];

    protected function tearDown(): void
    {
        parent::tearDown();
        foreach ($this->previous as $name => [$server, $env]) {
            if (null === $server) {
                unset($_SERVER[$name]);
            } else {
                $_SERVER[$name] = $server;
            }
            if (null === $env) {
                unset($_ENV[$name]);
            } else {
                $_ENV[$name] = $env;
            }
        }
    }

    public function testRefusesTheRepositorysDevelopmentSettings(): void
    {
        $tester = new CommandTester(new Application(self::bootKernel())->find('app:settings:check'));

        $tester->execute([]);

        self::assertSame(1, $tester->getStatusCode());
        $output = preg_replace('/\s+/', ' ', $tester->getDisplay());
        self::assertIsString($output);
        self::assertStringContainsString('APP_SECRET is still a development or example value', $output);
        self::assertStringContainsString('API_PUBLIC_URL must be an https address', $output);
        self::assertStringContainsString('CORS_ALLOW_ORIGIN must list origins separated by spaces', $output);
        self::assertStringContainsString('OAUTH_PASSPHRASE is still a development or example value', $output);
        self::assertStringContainsString('MAILER_DSN is not set to your mail server', $output);
        self::assertStringNotContainsString('$ecretf0rt3st', $output, 'No secret is printed.');
    }

    public function testAcceptsCompleteSettings(): void
    {
        $this->setEnvironment([
            'APP_SECRET' => str_repeat('s', 40),
            'DATABASE_URL' => 'postgresql://varve:'.str_repeat('p', 32).'@db.internal:5432/varve?serverVersion=18&charset=utf8',
            'API_PUBLIC_URL' => 'https://api.varve.example.com',
            'CORS_ALLOW_ORIGIN' => 'https://varve.example.com tauri://localhost http://tauri.localhost',
            'VARVE_BILLING' => '1',
            'OAUTH_PASSPHRASE' => str_repeat('o', 24),
            'OAUTH_ENCRYPTION_KEY' => str_repeat('k', 40),
            'MAILER_DSN' => 'smtp://varve:'.str_repeat('m', 16).'@smtp.example.com:587',
            'VARVE_MAIL_FROM' => 'Varve <accounts@varve.example.com>',
            'WEB_APP_URL' => 'https://varve.example.com',
            'REDIS_URL' => 'redis://redis:6379',
        ]);
        $tester = new CommandTester(new Application(self::bootKernel())->find('app:settings:check'));

        $tester->execute([]);

        self::assertSame(0, $tester->getStatusCode(), $tester->getDisplay());
        self::assertStringContainsString('The production settings are complete.', $tester->getDisplay());
    }

    /** @param array<string, string> $settings */
    private function setEnvironment(array $settings): void
    {
        foreach ($settings as $name => $value) {
            $this->previous[$name] = [$_SERVER[$name] ?? null, $_ENV[$name] ?? null];
            $_SERVER[$name] = $_ENV[$name] = $value;
        }
    }
}
