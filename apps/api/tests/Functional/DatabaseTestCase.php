<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Doctrine\DBAL\Connection;
use Psr\Cache\CacheItemPoolInterface;
use Symfony\Bundle\FrameworkBundle\KernelBrowser;
use Symfony\Bundle\FrameworkBundle\Test\WebTestCase;

/**
 * Base for HTTP tests that touch the database. DAMADoctrineTestBundle rolls every test back.
 */
abstract class DatabaseTestCase extends WebTestCase
{
    protected KernelBrowser $client;

    protected function setUp(): void
    {
        $this->client = static::createClient();
        $this->client->disableReboot();
    }

    protected function connection(): Connection
    {
        return static::getContainer()->get(Connection::class);
    }

    /**
     * The number a counting query returns.
     *
     * @param array<string, mixed> $parameters
     */
    protected function countRows(string $sql, array $parameters = []): int
    {
        $count = $this->connection()->fetchOne($sql, $parameters);
        self::assertIsInt($count);

        return $count;
    }

    /**
     * The JSON body of the last response; changes with every request.
     *
     * @phpstan-impure
     *
     * @return array<mixed>
     */
    protected function decodedResponse(): array
    {
        $decoded = json_decode((string) $this->client->getResponse()->getContent(), true, flags: \JSON_THROW_ON_ERROR);
        self::assertIsArray($decoded);

        return $decoded;
    }

    /** API_PUBLIC_URL, which the issuer of tokens and links in mail are built on. */
    protected function apiAddress(): string
    {
        $address = $_SERVER['API_PUBLIC_URL'] ?? null;
        self::assertIsString($address);

        return rtrim($address, '/');
    }

    /**
     * Forgets every rate limit: the sign-in form's, the account pages' and the password checks'. They live outside the
     * database, so they outlive a test, and a failed sign-in counts against the network address of every test client
     * (127.0.0.1) for 15 minutes.
     */
    protected function resetRateLimits(): void
    {
        $pool = static::getContainer()->get('cache.rate_limiter_shared');
        self::assertInstanceOf(CacheItemPoolInterface::class, $pool);
        self::assertTrue($pool->clear());
    }
}
