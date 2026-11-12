<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Symfony\Bundle\FrameworkBundle\Console\Application;
use Symfony\Component\Console\Tester\CommandTester;

final class CreateUserCommandTest extends DatabaseTestCase
{
    use AuthenticatesUsers;

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->resetRateLimits();
    }

    public function testCreatesAUserWhoCanSignInAtOnce(): void
    {
        $tester = $this->command();
        $tester->setInputs([self::DEFAULT_PASSWORD]);
        $tester->execute(['email' => ' Kim@Example.test ', 'name' => 'Kim Visser']);

        $tester->assertCommandIsSuccessful();
        self::assertStringContainsString('Created user kim@example.test', $tester->getDisplay());
        self::assertStringNotContainsString(self::DEFAULT_PASSWORD, $tester->getDisplay());
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = 'kim@example.test' AND email_confirmed_at IS NOT NULL"));
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM organization WHERE personal_owner_id IS NOT NULL'));
        self::assertEmailCount(0);

        $this->me($this->signInDevice('kim@example.test')['access_token']);
        self::assertResponseIsSuccessful();
        self::assertSame('Kim Visser', $this->decodedResponse()['displayName'] ?? null);
    }

    public function testAnAddressThatHasAnAccountIsAnError(): void
    {
        $this->createUser('kim@example.test', 'Kim Visser');

        $tester = $this->command();
        $tester->execute(['email' => 'KIM@example.test', 'name' => 'Someone Else']);

        self::assertSame(1, $tester->getStatusCode());
        self::assertStringContainsString('already exists', $tester->getDisplay());
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    public function testChecksTheAddressAndTheName(): void
    {
        foreach ([['not-an-address', 'Kim'], ['a..b@example.test', 'Kim'], ['kim@example.test', '']] as [$email, $name]) {
            $tester = $this->command();
            $tester->execute(['email' => $email, 'name' => $name]);

            self::assertSame(1, $tester->getStatusCode(), $email.' / '.$name);
        }
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    public function testAShortPasswordIsRefused(): void
    {
        $tester = $this->command();
        $tester->setInputs(['short', self::DEFAULT_PASSWORD]);
        $tester->execute(['email' => 'kim@example.test', 'name' => 'Kim Visser']);

        $tester->assertCommandIsSuccessful();
        self::assertStringContainsString('at least 12 characters', $tester->getDisplay());
        $this->signInDevice('kim@example.test');
    }

    private function command(): CommandTester
    {
        return new CommandTester(new Application($this->client->getKernel())->find('app:users:create'));
    }
}
