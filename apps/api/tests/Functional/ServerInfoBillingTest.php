<?php

declare(strict_types=1);

namespace App\Tests\Functional;

/**
 * A server that bills (VARVE_BILLING=1, as Varve Cloud) says so, with the trial a new organization gets.
 */
final class ServerInfoBillingTest extends DatabaseTestCase
{
    /** @var array{mixed, mixed} */
    private array $previous;

    protected function setUp(): void
    {
        $this->previous = [$_SERVER['VARVE_BILLING'] ?? null, $_ENV['VARVE_BILLING'] ?? null];
        $_SERVER['VARVE_BILLING'] = $_ENV['VARVE_BILLING'] = '1';
        parent::setUp();
    }

    protected function tearDown(): void
    {
        parent::tearDown();
        [$server, $env] = $this->previous;
        if (null === $server) {
            unset($_SERVER['VARVE_BILLING']);
        } else {
            $_SERVER['VARVE_BILLING'] = $server;
        }
        if (null === $env) {
            unset($_ENV['VARVE_BILLING']);
        } else {
            $_ENV['VARVE_BILLING'] = $env;
        }
    }

    public function testSaysThatThisServerBills(): void
    {
        $this->client->request('GET', '/api/v1/server-info');

        self::assertResponseIsSuccessful();
        self::assertSame(['product' => 'varve', 'apiVersion' => 1, 'billing' => true, 'trialDays' => 14], $this->decodedResponse());
    }
}
