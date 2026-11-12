<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Sync\ChangeParser;
use App\Tests\Unit\Sync\DesktopPayloads;
use PHPUnit\Framework\Attributes\DataProvider;

/**
 * What a push may carry: the envelope, the basics of each kind, sizes, and never a secret (design 8.8).
 */
final class SyncPayloadTest extends DatabaseTestCase
{
    use AuthenticatesUsers;
    use SyncsRecords;

    private const string USER_PUSH = '/api/v1/sync/user/push';

    /** @var array<string, string> */
    private array $kim;
    private string $push;

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->createUser('kim@example.test', 'Kim Visser');
        $this->kim = $this->userHeaders('kim@example.test');
        $this->push = self::organizationPath($this->organizationOf($this->kim), 'push');
    }

    /** @return iterable<string, array{array<string, mixed>, string}> */
    public static function secrets(): iterable
    {
        yield 'a password' => [['password' => 'hunter2'], 'payload.password'];
        yield 'the SSH password' => [['ssh' => ['enabled' => true, 'password' => 'hunter2']], 'payload.ssh.password'];
        yield 'a keychain kind as a field' => [['dbPassword' => 'hunter2'], 'payload.dbPassword'];
        yield 'the TLS key passphrase' => [['tls' => ['mode' => 'verify_full', 'tls_key_passphrase' => 'x']], 'payload.tls.tls_key_passphrase'];
        yield 'an SSH key passphrase' => [['ssh' => ['sshKeyPassphrase' => 'x']], 'payload.ssh.sshKeyPassphrase'];
        yield 'a private key' => [['ssh' => ['private_key' => 'AAAA']], 'payload.ssh.private_key'];
        yield 'a client secret' => [['options' => ['clientSecret' => 'x']], 'payload.options.clientSecret'];
        yield 'a token' => [['token' => 'x'], 'payload.token'];
        yield 'an IAM token' => [['options' => ['iamToken' => 'x']], 'payload.options.iamToken'];
        yield 'an API key' => [['api-key' => 'x'], 'payload.api-key'];
        yield 'a secret' => [['secret' => 'x'], 'payload.secret'];
        yield 'a number as a password' => [['password' => 1234], 'payload.password'];
        yield 'an empty password' => [['password' => ''], 'payload.password'];
        yield 'a password object' => [['password' => ['value' => 'x']], 'payload.password'];
        yield 'deep in a list' => [['options' => ['hops' => [['host' => 'a'], ['host' => 'b'], ['host' => 'c', 'pwd' => 'x']]]], 'payload.options.hops[2].pwd'];
        yield 'a PEM private key' => [['options' => ['initSql' => "-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----"]], 'payload.options.initSql'];
        yield 'an OpenSSH key' => [['notes' => "key:\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3Bl\n"], 'payload.notes'];
        yield 'a PGP private key' => [['notes' => '-----BEGIN PGP PRIVATE KEY BLOCK-----'], 'payload.notes'];
        yield 'a PuTTY key' => [['ssh' => ['identity' => "PuTTY-User-Key-File-3: ssh-ed25519\n"]], 'payload.ssh.identity'];
        yield 'a key' => [['ssh' => ['key' => 'AAAAB3NzaC1yc2EAAAADAQABAAABAQ']], 'payload.ssh.key'];
        yield 'a secret key' => [['options' => ['secretKey' => 'x']], 'payload.options.secretKey'];
        yield 'an AWS secret access key' => [['options' => ['aws_secret_access_key' => 'x']], 'payload.options.aws_secret_access_key'];
        yield 'an access key' => [['accessKey' => 'x'], 'payload.accessKey'];
        yield 'an SSH key' => [['ssh' => ['sshKey' => 'x']], 'payload.ssh.sshKey'];
        yield 'an encryption key' => [['encryptionKey' => 'x'], 'payload.encryptionKey'];
        yield 'a signing key' => [['signingKey' => 'x'], 'payload.signingKey'];
        yield 'a password spelled pwd' => [['dbPwd' => 'x'], 'payload.dbPwd'];
        yield 'an SSH password spelled pwd' => [['ssh' => ['ssh_pwd' => 'x']], 'payload.ssh.ssh_pwd'];
        yield 'a password spelled pass' => [['pass' => 'x'], 'payload.pass'];
        yield 'a password spelled pw' => [['pw' => 'x'], 'payload.pw'];
        yield 'a passcode' => [['passcode' => '1234'], 'payload.passcode'];
        yield 'a PIN' => [['pin' => 1234], 'payload.pin'];
        yield 'a list of passwords' => [['passwords' => ['a', 'b']], 'payload.passwords'];
        yield 'a list of tokens' => [['tokens' => ['a']], 'payload.tokens'];
        yield 'a list of passphrases' => [['passphrases' => ['a']], 'payload.passphrases'];
        yield 'a list of API keys' => [['apiKeys' => ['a']], 'payload.apiKeys'];
        yield 'a numbered password' => [['password1' => 'x'], 'payload.password1'];
        yield 'a password value' => [['passwordValue' => 'x'], 'payload.passwordValue'];
        yield 'a token value' => [['tokenValue' => 'x'], 'payload.tokenValue'];
        yield 'a secret value' => [['secretValue' => 'x'], 'payload.secretValue'];
        yield 'credentials' => [['credentials' => ['user' => 'kim']], 'payload.credentials'];
        yield 'a credential' => [['credential' => 'x'], 'payload.credential'];
        yield 'an authorization header' => [['options' => ['authorization' => 'Basic a2ltOmh1bnRlcjI=']], 'payload.options.authorization'];
        yield 'a bearer' => [['bearer' => 'x'], 'payload.bearer'];
        yield 'a cookie' => [['cookie' => 'session=x'], 'payload.cookie'];
        yield 'a connection string' => [['connectionString' => 'Server=db;Database=shop'], 'payload.connectionString'];
        yield 'a DSN' => [['dsn' => 'db.example.test'], 'payload.dsn'];
        yield 'a URL with a password' => [['url' => 'postgres://kim:hunter2@db.example.test/shop'], 'payload.url'];
        yield 'a URI with a password' => [['uri' => 'redis://:hunter2@cache.example.test'], 'payload.uri'];
        yield 'a host that is a URL with a password' => [['host' => 'postgres://kim:hunter2@db.example.test/shop'], 'payload.host'];
        yield 'a host with a user and a password' => [['host' => 'kim:hunter2@db.example.test'], 'payload.host'];
        yield 'a password in a URL parameter' => [['options' => ['jdbcUrl' => 'jdbc:mysql://db.example.test/shop?user=kim&password=hunter2']], 'payload.options.jdbcUrl'];
        yield 'a connection string in another field' => [['options' => ['conninfo' => 'host=db.example.test user=kim password=hunter2']], 'payload.options.conninfo'];
        yield 'a name and value pair' => [['options' => ['parameters' => [['name' => 'sslmode', 'value' => 'require'], ['name' => 'password', 'value' => 'hunter2']]]], 'payload.options.parameters[1].value'];
        yield 'a key and value pair' => [['options' => ['parameters' => [['key' => 'token', 'value' => 'x']]]], 'payload.options.parameters[0].value'];
        yield 'JSON in a string' => [['extra' => '{"password":"hunter2"}'], 'payload.extra'];
    }

    /**
     * @param array<string, mixed> $secret
     */
    #[DataProvider('secrets')]
    public function testASecretIsRefusedWithItsPath(array $secret, string $field): void
    {
        $id = self::newId();

        $result = $this->push($this->push, [self::upsert($id, 'connection', self::connectionProfile($id, $secret))], $this->kim)[0];

        self::assertSame('rejected', $result['status'] ?? null);
        self::assertSame('secret', $result['reason'] ?? null);
        self::assertSame($field, $result['field'] ?? null);
        self::assertStringStartsWith($field.' ', self::string($result['message'] ?? null));
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM record'));
    }

    public function testFieldsThatOnlyTalkAboutSecretsAreKept(): void
    {
        $id = self::newId();
        $profile = self::connectionProfile($id, [
            // Which secret kinds this device keeps in its keychain, as the desktop app writes it.
            'savedSecrets' => ['db_password', 'ssh_password', 'ssh_key_passphrase', 'tls_key_passphrase'],
            'secretKind' => 'db_password',
            'authMethod' => 'iam_token',
            'passwordSaved' => true,
            'password' => null,
            'askPassword' => false,
            'sslmode' => 'verify-full',
            'tls' => ['mode' => 'verify_full', 'caPath' => '/etc/ssl/ca.pem', 'clientCertPath' => '/home/kim/client.crt', 'clientKeyPath' => '/home/kim/client.key'],
            'ssh' => ['enabled' => true, 'host' => 'bastion.example.test', 'port' => 22, 'username' => 'kim', 'auth' => 'key_file', 'keyPath' => '~/.ssh/id_ed25519', 'privateKeyPath' => '~/.ssh/id_ed25519'],
            // Certificates and public keys are not secrets, and SQL may talk about passwords and addresses.
            'options' => ['initSql' => "SET search_path TO shop; -- mentions a password, which is fine\nSELECT 'BEGIN PRIVATE KEY', '-----BEGIN CERTIFICATE-----', 'postgres://kim:x@db/shop'"],
            'caCertificate' => "-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIUEXAMPLE\n-----END CERTIFICATE-----",
            'publicKey' => '-----BEGIN PUBLIC KEY-----',
            'parameters' => [['key' => 'sslmode', 'value' => 'require'], ['name' => 'password', 'value' => null]],
        ]);

        self::assertSame('accepted', $this->push($this->push, [self::upsert($id, 'connection', $profile)], $this->kim)[0]['status'] ?? null);
    }

    public function testUserRecordsAreGuardedToo(): void
    {
        $id = self::newId();

        $result = $this->push(self::USER_PUSH, [self::upsert($id, 'override', ['connectionId' => self::newId(), 'ssh' => ['passphrase' => 'x']])], $this->kim)[0];

        self::assertSame(['secret', 'payload.ssh.passphrase'], [$result['reason'] ?? null, $result['field'] ?? null]);
    }

    /** @return iterable<string, array{string, array<string, mixed>, string}> */
    public static function invalidPayloads(): iterable
    {
        yield 'a folder without a name' => ['folder', ['parentId' => null], 'payload.name'];
        yield 'a blank name' => ['folder', ['name' => '   '], 'payload.name'];
        yield 'a name that is too long' => ['folder', ['name' => str_repeat('n', 201)], 'payload.name'];
        yield 'a name that is not a string' => ['query', ['name' => 5, 'sql' => ''], 'payload.name'];
        yield 'a parent that is no id' => ['folder', ['name' => 'Webshop', 'parentId' => 'top'], 'payload.parentId'];
        yield 'an upper-case parent id' => ['folder', ['name' => 'Webshop', 'parentId' => '0196F3C2-0000-7000-8000-000000000000'], 'payload.parentId'];
        yield 'a color that is no string' => ['folder', ['name' => 'Webshop', 'color' => ['red']], 'payload.color'];
        yield 'an order that is no integer' => ['folder', ['name' => 'Webshop', 'sortOrder' => 1.5], 'payload.sortOrder'];
        yield 'a connection without an engine' => ['connection', ['name' => 'Shop'], 'payload.engine'];
        yield 'an odd engine' => ['connection', ['name' => 'Shop', 'engine' => 'Postgres 18'], 'payload.engine'];
        yield 'a port out of range' => ['connection', ['name' => 'Shop', 'engine' => 'postgres', 'port' => 70000], 'payload.port'];
        yield 'a host that is too long' => ['connection', ['name' => 'Shop', 'engine' => 'postgres', 'host' => str_repeat('h', 256)], 'payload.host'];
        yield 'a folder reference that is no id' => ['connection', ['name' => 'Shop', 'engine' => 'postgres', 'folderId' => 7], 'payload.folderId'];
        yield 'ssh that is no object' => ['connection', ['name' => 'Shop', 'engine' => 'postgres', 'ssh' => 'yes'], 'payload.ssh'];
        yield 'a query without sql' => ['query', ['name' => 'Orders'], 'payload.sql'];
        yield 'a query on a connection that is no id' => ['query', ['name' => 'Orders', 'sql' => 'SELECT 1', 'connectionId' => 'shop'], 'payload.connectionId'];
        yield 'tags that are no list' => ['query', ['name' => 'Orders', 'sql' => 'SELECT 1', 'tags' => 'orders'], 'payload.tags'];
        yield 'a blank tag' => ['query', ['name' => 'Orders', 'sql' => 'SELECT 1', 'tags' => ['orders', '']], 'payload.tags[1]'];
        yield 'a payload nested too deep' => ['folder', ['name' => 'Deep', 'x' => self::nested(33)], 'payload.x'.str_repeat('.a', 32)];
        yield 'a list nested too deep' => ['folder', ['name' => 'Deep', 'x' => self::nestedList(33)], 'payload.x'.str_repeat('[0]', 32)];
        yield 'a number beyond 64 bits' => ['folder', ['name' => 'Big', 'sortOrder' => 1, 'x' => 1e19], 'payload.x'];
    }

    /**
     * @param array<string, mixed> $payload
     */
    #[DataProvider('invalidPayloads')]
    public function testEachKindChecksItsBasics(string $kind, array $payload, string $field): void
    {
        $id = self::newId();

        $result = $this->push($this->push, [self::upsert($id, $kind, $payload)], $this->kim)[0];

        self::assertSame(['id' => $id, 'status' => 'rejected', 'reason' => 'invalid', 'field' => $field], array_intersect_key($result, array_flip(['id', 'status', 'reason', 'field'])), self::string($result['message'] ?? null));
    }

    public function testAPayloadNestsAtMost32LevelsBelowItself(): void
    {
        $id = self::newId();

        $results = $this->push($this->push, [
            self::upsert($id, 'folder', ['name' => 'Deep', 'x' => self::nested(32), 'y' => self::nestedList(32)]),
            self::upsert(self::newId(), 'folder', ['name' => 'Deeper', 'x' => self::nested(33)]),
        ], $this->kim);

        self::assertSame(['id' => $id, 'status' => 'accepted', 'version' => 1], $results[0], 'Level 32 is the last one allowed.');
        self::assertSame('payload.x'.str_repeat('.a', 32).' nests more than 32 levels below the payload.', $results[1]['message'] ?? null);
    }

    /**
     * 1e999 is valid JSON that no double can hold. It rejects its own change; the others of the push go on.
     */
    public function testANumberTooLargeForADoubleRejectsItsChangeOnly(): void
    {
        [$huge, $negative, $fine] = [self::newId(), self::newId(), self::newId()];
        $body = \sprintf(
            '{"changes":[{"op":"upsert","id":"%s","kind":"folder","baseVersion":0,"payload":{"name":"x","n":1e999}},'
            .'{"op":"upsert","id":"%s","kind":"folder","baseVersion":0,"payload":{"name":"x","list":[1,{"n":-1e999}]}},'
            .'{"op":"upsert","id":"%s","kind":"folder","baseVersion":0,"payload":{"name":"Fine","n":1e18}}]}',
            $huge, $negative, $fine,
        );

        $this->client->request('POST', $this->push, server: [...$this->kim, 'CONTENT_TYPE' => 'application/json'], content: $body);

        self::assertResponseIsSuccessful();
        $results = $this->decodedResponse()['results'] ?? null;
        self::assertSame([
            ['id' => $huge, 'status' => 'rejected', 'reason' => 'invalid', 'field' => 'payload.n', 'message' => 'payload.n is a number beyond a signed 64-bit integer.'],
            ['id' => $negative, 'status' => 'rejected', 'reason' => 'invalid', 'field' => 'payload.list[1].n', 'message' => 'payload.list[1].n is a number beyond a signed 64-bit integer.'],
            ['id' => $fine, 'status' => 'accepted', 'version' => 1],
        ], $results);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM record'));
    }

    /**
     * A key of a payload can be as long as the payload: the answer names it by its first 200 bytes.
     */
    public function testALongPathIsCutInTheAnswer(): void
    {
        $long = str_repeat('k', 5000);
        $cut = 'payload.'.str_repeat('k', 192).'...';

        $results = $this->push($this->push, [
            self::upsert(self::newId(), 'folder', ['name' => 'x', $long.'Password' => 'hunter2']),
            self::upsert(self::newId(), 'folder', ['name' => 'x', $long => 1e19]),
            self::upsert(self::newId(), 'folder', ['name' => 'x', $long => '-----BEGIN PRIVATE KEY-----']),
            ['op' => 'delete', 'id' => self::newId(), 'baseVersion' => 1, $long => true],
            self::upsert(self::newId(), 'folder', ['name' => 'x', str_repeat('é', 150) => ['token' => 'x']]),
        ], $this->kim);

        self::assertSame([$cut, $cut, $cut, str_repeat('k', 200).'...', 'payload.'.str_repeat('é', 96).'...'], array_column($results, 'field'));
        self::assertSame(['secret', 'invalid', 'secret', 'invalid', 'secret'], array_column($results, 'reason'));
        self::assertSame($cut.' looks like a secret. Passwords, passphrases, keys and tokens never leave the device.', $results[0]['message'] ?? null);
        foreach ($results as $result) {
            self::assertLessThan(400, \strlen(self::string($result['message'] ?? null)));
        }
    }

    /**
     * @param array<string, mixed> $payload
     */
    #[DataProvider('desktopPayloads')]
    public function testEveryPayloadOfTheDesktopAppIsStoredAsItIs(string $kind, array $payload): void
    {
        $id = self::string($payload['id'] ?? null);
        $user = \in_array($kind, ['favourite', 'override', 'preference'], true);

        $result = $this->push($user ? self::USER_PUSH : $this->push, [self::upsert($id, $kind, $payload)], $this->kim)[0];

        self::assertSame(['id' => $id, 'status' => 'accepted', 'version' => 1], $result);
        $stored = $this->connection()->fetchOne(\sprintf('SELECT payload::text FROM %s WHERE id = :id', $user ? 'user_record' : 'record'), ['id' => $id]);
        self::assertSame($payload, json_decode(self::string($stored), true, flags: \JSON_THROW_ON_ERROR));
    }

    /** @return iterable<string, array{string, array<string, mixed>}> */
    public static function desktopPayloads(): iterable
    {
        yield from DesktopPayloads::all(self::newId());
    }

    public function testAFolderIsNotItsOwnParentAndThePayloadIdIsTheRecords(): void
    {
        $id = self::newId();

        $results = $this->push($this->push, [
            self::upsert($id, 'folder', self::folderRecord($id, ['parentId' => $id])),
            self::upsert($id, 'folder', self::folderRecord(self::newId())),
        ], $this->kim);

        self::assertSame(['payload.parentId', 'payload.id'], array_column($results, 'field'));
    }

    public function testUserRecordsCheckTheirKinds(): void
    {
        $results = $this->push(self::USER_PUSH, [
            self::upsert(self::newId(), 'favourite', ['note' => 'no record']),
            self::upsert(self::newId(), 'override', ['connectionId' => 'shop']),
            self::upsert(self::newId(), 'folder', self::folderRecord(self::newId())),
            self::upsert(self::newId(), 'preference', ['anything' => ['goes' => true]]),
        ], $this->kim);

        self::assertSame(['invalid', 'invalid', 'unknown_kind', null], array_map(static fn (array $result): mixed => $result['reason'] ?? null, $results));
        self::assertSame(['payload.recordId', 'payload.connectionId', 'kind', null], array_map(static fn (array $result): mixed => $result['field'] ?? null, $results));
        self::assertSame('kind must be one of: favourite, preference, override.', $results[2]['message'] ?? null);
    }

    public function testTheEnvelopeOfEachChangeIsChecked(): void
    {
        $id = self::newId();
        $results = $this->push($this->push, [
            'not an object',
            ['op' => 'upsert', 'id' => 'NOT-A-UUID', 'kind' => 'folder', 'baseVersion' => 0, 'payload' => ['name' => 'x']],
            ['op' => 'replace', 'id' => $id, 'baseVersion' => 0],
            ['op' => 'upsert', 'id' => $id, 'kind' => 'folder', 'baseVersion' => -1, 'payload' => ['name' => 'x']],
            ['op' => 'upsert', 'id' => $id, 'kind' => 'folder', 'baseVersion' => '1', 'payload' => ['name' => 'x']],
            ['op' => 'upsert', 'id' => $id, 'kind' => 'job_profile', 'baseVersion' => 0, 'payload' => ['name' => 'x']],
            ['op' => 'upsert', 'id' => $id, 'kind' => 'folder', 'baseVersion' => 0, 'payload' => ['x']],
            ['op' => 'upsert', 'id' => $id, 'kind' => 'folder', 'baseVersion' => 0, 'payload' => ['name' => 'x'], 'deleted' => false],
            ['op' => 'delete', 'id' => $id, 'baseVersion' => 1, 'payload' => ['name' => 'x']],
            self::upsert($id, 'folder', ['name' => 'Fine']),
        ], $this->kim);

        self::assertSame(
            [[null, 'invalid', null], [null, 'invalid', 'id'], [$id, 'invalid', 'op'], [$id, 'invalid', 'baseVersion'], [$id, 'invalid', 'baseVersion'], [$id, 'unknown_kind', 'kind'], [$id, 'invalid', 'payload'], [$id, 'invalid', 'deleted'], [$id, 'invalid', 'payload'], [$id, null, null]],
            array_map(static fn (array $result): array => [$result['id'] ?? null, $result['reason'] ?? null, $result['field'] ?? null], $results),
        );
        self::assertSame('accepted', $results[9]['status'] ?? null, 'The other changes of the push go on.');
    }

    public function testAPayloadIsAtMost256KiB(): void
    {
        $id = self::newId();
        $base = self::savedQuery($id, ['sql' => '']);
        $padding = ChangeParser::MAX_PAYLOAD_BYTES - \strlen(json_encode($base, \JSON_THROW_ON_ERROR | \JSON_UNESCAPED_SLASHES | \JSON_UNESCAPED_UNICODE));
        $atTheLimit = self::savedQuery($id, ['sql' => str_repeat('x', $padding)]);
        $over = self::savedQuery($id, ['sql' => str_repeat('x', $padding + 1)]);

        $results = $this->push($this->push, [self::upsert($id, 'query', $over), self::upsert($id, 'query', $atTheLimit)], $this->kim);

        self::assertSame(['id' => $id, 'status' => 'rejected', 'reason' => 'too_large', 'field' => 'payload', 'message' => 'payload is 262145 bytes as JSON; at most 262144 are allowed.', 'limit' => 262144, 'size' => 262145], $results[0]);
        self::assertSame(['id' => $id, 'status' => 'accepted', 'version' => 1], $results[1]);
        self::assertSame(ChangeParser::MAX_PAYLOAD_BYTES, $this->countRows('SELECT octet_length(payload::text) FROM record'));
        self::assertSame(ChangeParser::MAX_PAYLOAD_BYTES, $this->countRows('SELECT payload_bytes FROM record'), 'The size is kept beside the payload.');
    }

    public function testAPushHoldsAtMost100Changes(): void
    {
        $changes = [];
        for ($i = 0; $i < 101; ++$i) {
            $id = self::newId();
            $changes[] = self::upsert($id, 'folder', self::folderRecord($id));
        }

        $this->client->jsonRequest('POST', $this->push, ['changes' => $changes], $this->kim);

        self::assertResponseStatusCodeSame(422);
        $violations = $this->decodedResponse()['violations'] ?? null;
        self::assertIsArray($violations);
        self::assertSame([['propertyPath' => 'changes', 'message' => 'A push can hold at most 100 changes.']], array_map(static fn (mixed $violation): array => \is_array($violation) ? array_intersect_key($violation, ['propertyPath' => 1, 'message' => 1]) : [], $violations));
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM record'), 'Nothing of it was stored.');

        self::assertCount(100, $this->push($this->push, \array_slice($changes, 0, 100), $this->kim));
        self::assertSame(100, $this->countRows('SELECT COUNT(*) FROM record'));
    }

    public function testABodyThatIsNotAPushFailsAsAWhole(): void
    {
        $this->client->request('POST', $this->push, server: [...$this->kim, 'CONTENT_TYPE' => 'application/json'], content: '{"changes": [');
        self::assertResponseStatusCodeSame(400);

        foreach (['{}', '[]', '{"changes": {"a": 1}}', '"changes"'] as $body) {
            $this->client->request('POST', $this->push, server: [...$this->kim, 'CONTENT_TYPE' => 'application/json'], content: $body);
            self::assertResponseStatusCodeSame(422, $body);
        }

        $this->client->request('POST', $this->push, server: [...$this->kim, 'CONTENT_TYPE' => 'text/plain'], content: '{"changes": []}');
        self::assertResponseStatusCodeSame(415);

        $this->client->request('POST', $this->push, server: [...$this->kim, 'CONTENT_TYPE' => 'application/json'], content: '{"changes": [], "padding": "'.str_repeat('x', ChangeParser::MAX_BODY_BYTES).'"}');
        self::assertResponseStatusCodeSame(413);
    }

    /**
     * A body that announces more than 8 MiB is refused on that, before it is read: PHP itself drops a body larger than
     * its post_max_size, and the answer must still be a 413.
     */
    public function testABodyIsRefusedOnItsAnnouncedLength(): void
    {
        foreach ([(string) (ChangeParser::MAX_BODY_BYTES + 1), '99999999999999999999'] as $length) {
            $this->client->request('POST', $this->push, server: [...$this->kim, 'CONTENT_TYPE' => 'application/json', 'CONTENT_LENGTH' => $length], content: '');
            self::assertResponseStatusCodeSame(413, $length);
            self::assertSame('A push can be at most 8388608 bytes; send the changes in more than one push.', $this->decodedResponse()['detail'] ?? null);
        }

        $this->client->request('POST', $this->push, server: [...$this->kim, 'CONTENT_TYPE' => 'application/json', 'CONTENT_LENGTH' => (string) ChangeParser::MAX_BODY_BYTES], content: '{"changes": []}');
        self::assertResponseIsSuccessful('A body of exactly the limit is read.');
    }

    /**
     * JSON may name a field with a NUL character in front; PHP cannot hold that as an object. The push fails as a
     * whole, with a 422 that says why.
     */
    public function testAFieldNameStartingWithNulFailsThePush(): void
    {
        $id = self::newId();
        $body = '{"changes":[{"op":"upsert","id":"'.$id.'","kind":"folder","baseVersion":0,"payload":{"name":"x","\u0000a":1}}]}';

        $this->client->request('POST', $this->push, server: [...$this->kim, 'CONTENT_TYPE' => 'application/json'], content: $body);

        self::assertResponseStatusCodeSame(422);
        self::assertSame('changes: The name of a field cannot start with a NUL character (\u0000).', $this->decodedResponse()['detail'] ?? null);
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM record'));
    }

    /**
     * This many objects, each inside the one before.
     *
     * @return array<string, mixed>
     */
    private static function nested(int $levels): array
    {
        return 1 === $levels ? ['a' => 1] : ['a' => self::nested($levels - 1)];
    }

    /**
     * This many lists, each inside the one before.
     *
     * @return list<mixed>
     */
    private static function nestedList(int $levels): array
    {
        return 1 === $levels ? [1] : [self::nestedList($levels - 1)];
    }
}
