<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Sync\ChangeParser;
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
        yield 'a certificate' => [['tls' => ['ca' => '-----BEGIN CERTIFICATE-----']], 'payload.tls.ca'];
        yield 'a PuTTY key' => [['ssh' => ['key' => "PuTTY-User-Key-File-3: ssh-ed25519\n"]], 'payload.ssh.key'];
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
            'options' => ['initSql' => "SET search_path TO shop; -- mentions a password, which is fine\nSELECT 'BEGIN PRIVATE KEY'"],
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
        yield 'a payload nested too deep' => ['folder', ['name' => 'Deep', 'x' => self::nested(33)], 'payload.x'.str_repeat('.a', 31)];
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

        self::assertSame(['id' => $id, 'status' => 'rejected', 'reason' => 'too_large', 'field' => 'payload', 'message' => 'payload is 262145 bytes as JSON; at most 262144 are allowed.'], $results[0]);
        self::assertSame(['id' => $id, 'status' => 'accepted', 'version' => 1], $results[1]);
        self::assertSame(ChangeParser::MAX_PAYLOAD_BYTES, $this->countRows('SELECT octet_length(payload::text) FROM record'));
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

    /** @return array<string, mixed> */
    private static function nested(int $levels): array
    {
        return 1 === $levels ? ['a' => 1] : ['a' => self::nested($levels - 1)];
    }
}
