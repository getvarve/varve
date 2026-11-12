<?php

declare(strict_types=1);

namespace App\Tests\Functional;

/**
 * A server that does not bill (every self-hosted server, and the default).
 */
final class ServerInfoTest extends DatabaseTestCase
{
    public function testSaysWhatThisServerIsBeforeSignIn(): void
    {
        $this->client->request('GET', '/api/v1/server-info');

        self::assertResponseIsSuccessful();
        self::assertResponseHeaderSame('Content-Type', 'application/json');
        self::assertSame(['product' => 'varve', 'apiVersion' => 1, 'billing' => false, 'trialDays' => 0, 'signUp' => true], $this->decodedResponse());
    }
}
