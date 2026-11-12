<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Controller\HealthController;
use Doctrine\DBAL\Connection;
use Symfony\Bundle\FrameworkBundle\Test\WebTestCase;

final class HealthTest extends WebTestCase
{
    public function testReportsDependencies(): void
    {
        $client = static::createClient();
        $client->request('GET', '/health');

        self::assertResponseIsSuccessful();
        self::assertJsonStringEqualsJsonString(
            '{"status":"ok","checks":{"database":"ok"}}',
            (string) $client->getResponse()->getContent(),
        );
    }

    public function testReportsRedisWhereTheServerUsesIt(): void
    {
        self::bootKernel();
        $redis = self::createStub(\Redis::class);
        $redis->method('ping')->willReturn(true);

        $response = new HealthController(self::getContainer()->get(Connection::class), $redis)();

        self::assertSame(200, $response->getStatusCode());
        self::assertJsonStringEqualsJsonString('{"status":"ok","checks":{"database":"ok","redis":"ok"}}', (string) $response->getContent());
    }

    public function testIsUnhealthyWhileRedisDoesNotAnswer(): void
    {
        self::bootKernel();
        $redis = self::createStub(\Redis::class);
        $redis->method('ping')->willThrowException(new \RedisException('Connection refused'));

        $response = new HealthController(self::getContainer()->get(Connection::class), $redis)();

        self::assertSame(503, $response->getStatusCode());
        self::assertJsonStringEqualsJsonString('{"status":"degraded","checks":{"database":"ok","redis":"failing"}}', (string) $response->getContent());
    }

    public function testNeedsNoSignIn(): void
    {
        $client = static::createClient();
        $client->request('GET', '/health', server: ['HTTP_AUTHORIZATION' => 'Bearer not-a-token']);

        self::assertResponseIsSuccessful();
    }
}
