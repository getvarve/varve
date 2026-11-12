<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use PHPUnit\Framework\Attributes\DataProvider;
use Symfony\Bundle\FrameworkBundle\KernelBrowser;
use Symfony\Bundle\FrameworkBundle\Test\WebTestCase;

final class CorsTest extends WebTestCase
{
    /**
     * @return iterable<string, array{string}>
     */
    public static function clientOrigins(): iterable
    {
        yield 'web client in development' => ['http://localhost:5173'];
        yield 'desktop app on macOS and Linux' => ['tauri://localhost'];
        yield 'desktop app on Windows' => ['http://tauri.localhost'];
    }

    #[DataProvider('clientOrigins')]
    public function testAllowsTheClientApps(string $origin): void
    {
        $client = static::createClient();
        $this->preflight($client, $origin);

        self::assertResponseIsSuccessful();
        self::assertResponseHeaderSame('Access-Control-Allow-Origin', $origin);
        self::assertStringContainsStringIgnoringCase('authorization', (string) $client->getResponse()->headers->get('Access-Control-Allow-Headers'));
    }

    /**
     * @return iterable<string, array{string}>
     */
    public static function otherOrigins(): iterable
    {
        yield 'another site' => ['https://evil.example'];
        yield 'another scheme on localhost' => ['tauri://localhost.evil.example'];
        yield 'a look-alike host' => ['http://tauri.localhost.evil.example'];
    }

    #[DataProvider('otherOrigins')]
    public function testRefusesOtherOrigins(string $origin): void
    {
        $client = static::createClient();
        $this->preflight($client, $origin);

        self::assertFalse($client->getResponse()->headers->has('Access-Control-Allow-Origin'));
    }

    private function preflight(KernelBrowser $client, string $origin): void
    {
        $client->request('OPTIONS', '/api/v1/server-info', server: [
            'HTTP_ORIGIN' => $origin,
            'HTTP_ACCESS_CONTROL_REQUEST_METHOD' => 'GET',
            'HTTP_ACCESS_CONTROL_REQUEST_HEADERS' => 'authorization',
        ]);
    }
}
