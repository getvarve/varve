<?php

declare(strict_types=1);

namespace App\Tests\Unit\Sync;

use App\Sync\SecretGuard;
use PHPUnit\Framework\Attributes\DataProvider;
use PHPUnit\Framework\TestCase;

final class SecretGuardTest extends TestCase
{
    /** @return iterable<string, array{string}> */
    public static function secretNames(): iterable
    {
        // The desktop app's keychain kinds (crates/secrets), in every spelling a client might use.
        foreach (['db_password', 'dbPassword', 'ssh_password', 'sshPassword', 'ssh_key_passphrase', 'sshKeyPassphrase', 'tls_key_passphrase', 'tlsKeyPassphrase'] as $name) {
            yield $name => [$name];
        }
        foreach (['password', 'Password', 'PASSWORD', 'passwd', 'pwd', 'passphrase', 'private_key', 'privateKey', 'private-key', 'secret', 'client_secret', 'clientSecret', 'token', 'accessToken', 'refresh_token', 'iamToken', 'apiKey', 'api_key'] as $name) {
            yield $name => [$name];
        }
    }

    #[DataProvider('secretNames')]
    public function testSecretNames(string $name): void
    {
        self::assertTrue(SecretGuard::isSecretName($name));
    }

    /** @return iterable<string, array{string}> */
    public static function harmlessNames(): iterable
    {
        foreach (['savedSecrets', 'saved_secrets', 'secretKind', 'secrets', 'passwordSaved', 'sslmode', 'sslMode', 'privateKeyPath', 'keyPath', 'clientKeyPath', 'client_key_path', 'tokenType', 'authMethod', 'username', 'host', 'pwdHint'] as $name) {
            yield $name => [$name];
        }
    }

    #[DataProvider('harmlessNames')]
    public function testHarmlessNames(string $name): void
    {
        self::assertFalse(SecretGuard::isSecretName($name));
    }

    public function testASecretNameMayHoldAFlagOrNothing(): void
    {
        $payload = json_decode('{"password": null, "ssh": {"passphrase": true, "token": false}}', false, flags: \JSON_THROW_ON_ERROR);

        self::assertNull(SecretGuard::find($payload));
    }

    public function testThePathNamesTheFirstSecret(): void
    {
        $payload = json_decode('{"a": [{"b": 1}, {"tls": {"keyPassphrase": "x"}}], "password": "y"}', false, flags: \JSON_THROW_ON_ERROR);

        self::assertSame('payload.a[1].tls.keyPassphrase', SecretGuard::find($payload)['field'] ?? null);
    }

    public function testPemBlocksAnywhere(): void
    {
        foreach (['-----BEGIN RSA PRIVATE KEY-----', "x\n-----BEGIN EC PRIVATE KEY-----\n", '-----BEGIN ENCRYPTED PRIVATE KEY-----', '-----BEGIN CERTIFICATE-----'] as $text) {
            self::assertSame('payload.list[0]', SecretGuard::find(json_decode((string) json_encode(['list' => [$text]]), false, flags: \JSON_THROW_ON_ERROR))['field'] ?? null, $text);
        }
        self::assertNull(SecretGuard::find(json_decode('{"sql": "SELECT \'-----BEGIN\' AS banner"}', false, flags: \JSON_THROW_ON_ERROR)));
    }
}
