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
        // Keys of every sort.
        foreach (['secretKey', 'secret_access_key', 'aws_secret_access_key', 'accessKey', 'sshKey', 'encryptionKey', 'signingKey', 'key', 'Key', 'privateKeys', 'apiKeys'] as $name) {
            yield $name => [$name];
        }
        // Short and plural spellings, and a secret word followed by a number or by "value".
        foreach (['dbPwd', 'ssh_pwd', 'userPwd', 'pass', 'pw', 'passcode', 'pin', 'passwords', 'tokens', 'passphrases', 'password1', 'password_2', 'passwordValue', 'tokenValue', 'secretValue', 'secret_value_1'] as $name) {
            yield $name => [$name];
        }
        // What signs a request in, and whole connection strings.
        foreach (['credentials', 'credential', 'awsCredentials', 'authorization', 'Authorization', 'bearer', 'cookie', 'connectionString', 'connection_string', 'dsn', 'databaseDsn'] as $name) {
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
        // Every field name of the desktop app's records (crates/records/src/model.rs).
        foreach ([
            'id', 'workspaceId', 'parentId', 'folderId', 'connectionId', 'name', 'color', 'sortOrder', 'createdAt', 'updatedAt', 'version',
            'engine', 'port', 'database', 'environment', 'tls', 'ssh', 'options', 'mode', 'caPath', 'clientCertPath', 'enabled', 'auth',
            'readOnly', 'confirmDestructive', 'statementTimeoutMs', 'keepaliveS', 'initSql', 'showSystemObjects', 'redisDb', 'sql', 'tags', 'recordId',
        ] as $name) {
            yield $name => [$name];
        }
        // Words that only hold a secret word inside them.
        foreach (['keyBindings', 'primaryKey', 'foreignKey', 'sortKey', 'shortcutKey', 'compass', 'bypass', 'spin', 'value', 'cookies', 'keys', 'pins', 'askForPasswordEveryTime'] as $name) {
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
        $payload = json_decode('{"password": null, "ssh": {"passphrase": true, "token": false}, "key": null}', false, flags: \JSON_THROW_ON_ERROR);

        self::assertNull(SecretGuard::find($payload));
    }

    public function testThePathNamesTheFirstSecret(): void
    {
        $payload = json_decode('{"a": [{"b": 1}, {"tls": {"keyPassphrase": "x"}}], "password": "y"}', false, flags: \JSON_THROW_ON_ERROR);

        self::assertSame('payload.a[1].tls.keyPassphrase', SecretGuard::find($payload)['field'] ?? null);
    }

    public function testPrivateKeysAnywhere(): void
    {
        foreach ([
            '-----BEGIN PRIVATE KEY-----',
            '-----BEGIN RSA PRIVATE KEY-----',
            "x\n-----BEGIN EC PRIVATE KEY-----\n",
            '-----BEGIN ENCRYPTED PRIVATE KEY-----',
            '-----BEGIN OPENSSH PRIVATE KEY-----',
            '-----BEGIN PGP PRIVATE KEY BLOCK-----',
            "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none",
        ] as $text) {
            self::assertSame('payload.list[0]', self::find(['list' => [$text]])['field'] ?? null, $text);
        }
    }

    public function testCertificatesAndPublicKeysAreNotSecrets(): void
    {
        foreach ([
            '-----BEGIN CERTIFICATE-----',
            '-----BEGIN PUBLIC KEY-----',
            '-----BEGIN RSA PUBLIC KEY-----',
            '-----BEGIN PGP PUBLIC KEY BLOCK-----',
            '-----BEGIN CERTIFICATE REQUEST-----',
            '-----BEGIN X509 CRL-----',
            'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAEXAMPLE kim@laptop',
            "SELECT '-----BEGIN' AS banner, 'PRIVATE KEY' AS words",
        ] as $text) {
            self::assertNull(self::find(['sql' => $text, 'options' => ['initSql' => $text], 'list' => [$text]]), $text);
        }
    }

    /** @return iterable<string, array{string}> */
    public static function addressesWithAPassword(): iterable
    {
        yield 'a database URL' => ['postgres://kim:hunter2@db.example.test/shop'];
        yield 'a URL with upper case and a port' => ['MySQL://root:hunter2@127.0.0.1:3306/shop?ssl=true'];
        yield 'a URL with only a password' => ['redis://:hunter2@cache.example.test:6379/0'];
        yield 'a JDBC URL with user info' => ['jdbc:postgresql://kim:hunter2@db.example.test/shop'];
        yield 'a URL after white space' => ["  mongodb+srv://kim:hunter2@cluster.example.test/\n"];
        yield 'a host with user info' => ['kim:hunter2@db.example.test'];
        yield 'a host with user info and a port' => ['kim:hunter2@db.example.test:5432/shop'];
        yield 'a password as a URL parameter' => ['jdbc:mysql://db.example.test:3306/shop?user=kim&password=hunter2'];
        yield 'a password as the first URL parameter' => ['postgresql://db.example.test/shop?password=hunter2&sslmode=require'];
        yield 'a libpq connection string' => ['host=db.example.test port=5432 dbname=shop user=kim password=hunter2'];
        yield 'a libpq connection string with quotes' => ["host=db.example.test user=kim password='hunter 2' sslmode=require"];
        yield 'an ADO.NET connection string' => ['Server=db.example.test;Database=shop;User Id=kim;Password=hunter2;'];
        yield 'an ODBC connection string' => ['Driver={PostgreSQL Unicode};Server=db.example.test;Port=5432;Uid=kim;Pwd=hunter2'];
    }

    #[DataProvider('addressesWithAPassword')]
    public function testAnAddressWithAPasswordIsRefusedInAnyField(string $address): void
    {
        foreach (['host', 'url', 'uri', 'notes'] as $field) {
            self::assertSame('payload.'.$field, self::find(['name' => 'Shop', $field => $address])['field'] ?? null, $field);
        }
        self::assertSame('payload.options.servers[1]', self::find(['options' => ['servers' => ['db.example.test', $address]]])['field'] ?? null);
    }

    /** @return iterable<string, array{string}> */
    public static function harmlessText(): iterable
    {
        yield 'a host name' => ['db.shop.example.test'];
        yield 'a host with a port' => ['db.shop.example.test:5432'];
        yield 'an IPv6 address' => ['2001:db8::10'];
        yield 'a URL without user info' => ['postgres://db.example.test:5432/shop?sslmode=require'];
        yield 'a URL with a user only' => ['postgres://kim@db.example.test/shop'];
        yield 'a URL with an IPv6 host' => ['postgres://[2001:db8::10]:5432/shop'];
        yield 'a user at a tenant' => ['deploy@shop'];
        yield 'an SCP-style address' => ['git@example.test:shop/schema.git'];
        yield 'a Windows path' => ['C:\\Users\\kim\\.ssh\\id_ed25519'];
        yield 'a libpq connection string without a password' => ['host=db.example.test port=5432 dbname=shop user=kim sslmode=require'];
        yield 'SQL that sets things' => ["SET search_path = shop; SET application_name = 'varve';"];
        yield 'SQL that sets a password' => ["SET sql_mode = ''; SET PASSWORD = 'x';"];
        yield 'SQL that compares a password' => ["SELECT * FROM users WHERE host = 'a' AND password = 'x'"];
        yield 'SQL that uses a URL with a password' => ["SELECT * FROM dblink('postgres://report:report@replica/shop', 'SELECT 1') AS t(one int)"];
        yield 'SQL that uses a connection string' => ["SELECT * FROM dblink('host=replica user=report password=report', 'SELECT 1') AS t(one int)"];
        yield 'a time and a place' => ['12:30 @ the office'];
        yield 'JSON without a secret' => ['{"host": "db.example.test", "password": null, "savedSecrets": ["db_password"]}'];
        yield 'text that only starts like JSON' => ['{password: hunter2'];
        yield 'a JSON list of words' => ['["password", "token"]'];
    }

    #[DataProvider('harmlessText')]
    public function testHarmlessText(string $text): void
    {
        self::assertNull(self::find(['name' => 'Shop', 'host' => $text, 'sql' => $text, 'options' => ['initSql' => $text], 'tags' => [$text]]));
    }

    public function testANameAndValuePairIsCheckedLikeAField(): void
    {
        self::assertSame('payload.options.parameters[1].value', self::find(['options' => ['parameters' => [
            ['name' => 'sslmode', 'value' => 'require'],
            ['name' => 'password', 'value' => 'hunter2'],
        ]]])['field'] ?? null);
        self::assertSame('payload.parameters[0].value', self::find(['parameters' => [['key' => 'api_key', 'value' => 'abc123']]])['field'] ?? null);
        self::assertSame('payload.parameters[0].value', self::find(['parameters' => [['key' => 'dbPassword', 'value' => 1234]]])['field'] ?? null);

        self::assertNull(self::find(['parameters' => [
            ['key' => 'sslmode', 'value' => 'require'],
            ['key' => 'application_name', 'value' => 'varve'],
            ['name' => 'password', 'value' => null],
            ['name' => 'password', 'value' => true],
            ['name' => 'passwordSaved', 'value' => 'yes'],
        ]]), 'A pair is harmless unless its name is a secret and its value holds something.');
        self::assertSame('payload.ssh.key', self::find(['ssh' => ['key' => 'AAAAB3NzaC1yc2EAAAADAQABAAABAQ']])['field'] ?? null, 'Without a value beside it, key is a field that holds a key.');
    }

    public function testJsonInsideAStringIsRead(): void
    {
        $found = self::find(['extra' => '{"password":"hunter2"}']);
        self::assertSame(['field' => 'payload.extra', 'message' => 'payload.extra holds JSON with a secret in it. payload.extra.password looks like a secret. Passwords, passphrases, keys and tokens never leave the device.'], $found);

        self::assertSame('payload.options.raw', self::find(['options' => ['raw' => " [\n {\"host\": \"a\"}, {\"ssh\": {\"passphrase\": \"x\"}}]"]])['field'] ?? null);
        self::assertSame('payload.extra', self::find(['extra' => (string) json_encode(['inner' => (string) json_encode(['url' => 'postgres://kim:hunter2@db/shop'])])])['field'] ?? null, 'JSON in a string in JSON in a string.');
    }

    /**
     * @param array<string, mixed> $payload
     */
    #[DataProvider('desktopPayloads')]
    public function testEveryPayloadOfTheDesktopAppPasses(array $payload): void
    {
        self::assertNull(self::find($payload));
    }

    /** @return iterable<string, array{array<string, mixed>}> */
    public static function desktopPayloads(): iterable
    {
        foreach (DesktopPayloads::all('0196f3c2-9999-7000-8000-000000000009') as $name => [, $payload]) {
            yield $name => [$payload];
        }
    }

    /**
     * What the guard finds in a payload as it arrives: decoded from JSON, with objects as objects.
     *
     * @param array<string, mixed> $payload
     *
     * @return array{field: string, message: string}|null
     */
    private static function find(array $payload): ?array
    {
        return SecretGuard::find(json_decode(json_encode($payload, \JSON_THROW_ON_ERROR), false, flags: \JSON_THROW_ON_ERROR));
    }
}
