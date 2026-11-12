<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Symfony\Bundle\FrameworkBundle\Console\Application;
use Symfony\Component\Console\Tester\CommandTester;

/**
 * A server whose operator closed sign-up (VARVE_SIGN_UP=0): nobody creates an account by themselves, and the operator
 * adds users with `app:users:create`.
 */
final class SignUpClosedTest extends DatabaseTestCase
{
    use AuthenticatesUsers;

    /** @var array{mixed, mixed} */
    private array $previous;

    protected function setUp(): void
    {
        $this->previous = [$_SERVER['VARVE_SIGN_UP'] ?? null, $_ENV['VARVE_SIGN_UP'] ?? null];
        $_SERVER['VARVE_SIGN_UP'] = $_ENV['VARVE_SIGN_UP'] = '0';
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->resetRateLimits();
    }

    protected function tearDown(): void
    {
        parent::tearDown();
        [$server, $env] = $this->previous;
        if (null === $server) {
            unset($_SERVER['VARVE_SIGN_UP']);
        } else {
            $_SERVER['VARVE_SIGN_UP'] = $server;
        }
        if (null === $env) {
            unset($_ENV['VARVE_SIGN_UP']);
        } else {
            $_ENV['VARVE_SIGN_UP'] = $env;
        }
    }

    public function testTheSignUpPageAndItsLinksSayThatSignUpIsClosed(): void
    {
        foreach (['/register', '/confirm-email?token='.str_repeat('a', 64), '/confirm-email/resend'] as $page) {
            $this->client->request('GET', $page);

            self::assertResponseStatusCodeSame(403, $page);
            self::assertSelectorTextContains('h1', 'Sign-up is closed on this server');
            self::assertResponseHeaderSame('X-Frame-Options', 'DENY');
        }
    }

    public function testNobodySignsUp(): void
    {
        $this->client->request('GET', '/login');
        $this->client->request('POST', '/register', ['email' => 'noor@example.test', 'name' => 'Noor', 'password' => 'a-long-enough-password', '_csrf_token' => 'any']);

        self::assertResponseStatusCodeSame(403);
        self::assertSelectorTextContains('h1', 'Sign-up is closed on this server');
        self::assertEmailCount(0);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM app_user'));
    }

    public function testTheSignInPageDoesNotOfferAnAccount(): void
    {
        $crawler = $this->client->request('GET', '/login');

        self::assertResponseIsSuccessful();
        self::assertSame(0, $crawler->filter('a[href="/register"]')->count());
        self::assertSame(1, $crawler->filter('a[href="/forgot-password"]')->count(), 'A forgotten password is still reset.');
    }

    public function testServerInfoSaysSo(): void
    {
        $this->client->request('GET', '/api/v1/server-info');

        self::assertResponseIsSuccessful();
        self::assertSame(['product' => 'varve', 'apiVersion' => 1, 'billing' => false, 'trialDays' => 0, 'signUp' => false], $this->decodedResponse());
    }

    public function testTheOperatorStillAddsUsers(): void
    {
        $tester = new CommandTester(new Application($this->client->getKernel())->find('app:users:create'));
        $tester->setInputs([self::DEFAULT_PASSWORD]);
        $tester->execute(['email' => 'kim@example.test', 'name' => 'Kim Visser']);
        $tester->assertCommandIsSuccessful();

        $this->me($this->signInDevice('kim@example.test')['access_token']);
        self::assertResponseIsSuccessful();
    }
}
