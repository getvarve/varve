<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Symfony\Bundle\FrameworkBundle\Console\Application;
use Symfony\Component\Console\Tester\CommandTester;

final class DeleteUserCommandTest extends DatabaseTestCase
{
    use AuthenticatesUsers;

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->createUser('kim@example.test', 'Kim Visser');
        $this->createUser('lisa@example.test', 'Lisa Bakker');
    }

    public function testDeletesTheUserWithEverythingTheyOwnAndTheirTokens(): void
    {
        $kim = $this->signInDevice('kim@example.test');
        $lisa = $this->signInDevice('lisa@example.test');

        $tester = $this->command();
        $tester->execute(['email' => 'KIM@example.test', '--force' => true]);

        $tester->assertCommandIsSuccessful();
        self::assertStringContainsString('Deleted KIM@example.test', $tester->getDisplay());
        self::assertSame(0, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'kim@example.test'"));
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization'));
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization_member'));
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM device'));
        self::assertSame(0, $this->countRows("SELECT COUNT(*) FROM oauth2_authorization_code WHERE user_identifier = 'kim@example.test'"));
        $this->me($kim['access_token']);
        self::assertResponseStatusCodeSame(401);
        $this->me($lisa['access_token']);
        self::assertResponseIsSuccessful();
    }

    public function testAsksFirst(): void
    {
        $tester = $this->command();
        $tester->setInputs(['no']);
        $tester->execute(['email' => 'kim@example.test']);

        self::assertSame(1, $tester->getStatusCode());
        self::assertStringContainsString('Nothing deleted.', $tester->getDisplay());
        self::assertSame(2, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    public function testAnUnknownAddressIsAnError(): void
    {
        $tester = $this->command();
        $tester->execute(['email' => 'nobody@example.test', '--force' => true]);

        self::assertSame(1, $tester->getStatusCode());
        self::assertStringContainsString('No user has the email address', $tester->getDisplay());
    }

    private function command(): CommandTester
    {
        return new CommandTester(new Application($this->client->getKernel())->find('app:users:delete'));
    }
}
