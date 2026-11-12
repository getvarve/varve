<?php

declare(strict_types=1);

namespace App\Tests\Functional;

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

    public function testNeedsNoSignIn(): void
    {
        $client = static::createClient();
        $client->request('GET', '/health', server: ['HTTP_AUTHORIZATION' => 'Bearer not-a-token']);

        self::assertResponseIsSuccessful();
    }
}
