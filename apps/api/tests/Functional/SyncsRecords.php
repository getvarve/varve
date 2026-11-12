<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Symfony\Bundle\FrameworkBundle\KernelBrowser;
use Symfony\Component\Uid\Uuid;

/**
 * Pushes and pulls synced records the way the desktop app does, with payloads shaped like its records
 * (crates/records).
 *
 * @property KernelBrowser $client
 */
trait SyncsRecords
{
    /**
     * The personal organization of the user behind these headers.
     *
     * @param array<string, string> $headers
     */
    protected function organizationOf(array $headers): string
    {
        $this->client->request('GET', '/api/v1/me', server: $headers);
        self::assertResponseIsSuccessful();
        $organization = $this->decodedResponse()['personalOrganization'] ?? null;
        self::assertIsArray($organization);

        return self::string($organization['id'] ?? null);
    }

    protected static function organizationPath(string $organization, string $action): string
    {
        return '/api/v1/sync/organizations/'.$organization.'/'.$action;
    }

    /**
     * Pushes changes and returns the results, after checking the push itself was answered.
     *
     * @param list<mixed>           $changes
     * @param array<string, string> $headers
     *
     * @return list<array<string, mixed>>
     */
    protected function push(string $path, array $changes, array $headers): array
    {
        $this->client->jsonRequest('POST', $path, ['changes' => $changes], $headers);
        self::assertResponseIsSuccessful((string) $this->client->getResponse()->getContent());
        $results = $this->decodedResponse()['results'] ?? null;
        self::assertIsArray($results);
        self::assertCount(\count($changes), $results);

        /** @var list<array<string, mixed>> */
        return $results;
    }

    /**
     * One page of changes.
     *
     * @param array<string, string> $headers
     *
     * @return array{changes: list<array<string, mixed>>, cursor: string, hasMore: bool}
     */
    protected function pull(string $path, array $headers, ?string $cursor = null, ?int $limit = null): array
    {
        $query = array_filter(['cursor' => $cursor, 'limit' => $limit], static fn (mixed $value): bool => null !== $value);
        $this->client->request('GET', $path.([] === $query ? '' : '?'.http_build_query($query)), server: $headers);
        self::assertResponseIsSuccessful((string) $this->client->getResponse()->getContent());
        $page = $this->decodedResponse();
        self::assertIsArray($page['changes'] ?? null);
        self::assertIsString($page['cursor'] ?? null);
        self::assertIsBool($page['hasMore'] ?? null);

        /** @var array{changes: list<array<string, mixed>>, cursor: string, hasMore: bool} */
        return $page;
    }

    /**
     * Every page from a cursor on.
     *
     * @param array<string, string> $headers
     *
     * @return array{changes: list<array<string, mixed>>, cursor: string, pages: int}
     */
    protected function pullAll(string $path, array $headers, ?string $cursor = null, ?int $limit = null): array
    {
        $changes = [];
        $pages = 0;
        do {
            $page = $this->pull($path, $headers, $cursor, $limit);
            $changes = [...$changes, ...$page['changes']];
            $cursor = $page['cursor'];
            ++$pages;
        } while ($page['hasMore']);

        return ['changes' => $changes, 'cursor' => $cursor, 'pages' => $pages];
    }

    /**
     * @param array<string, mixed> $payload
     *
     * @return array<string, mixed>
     */
    protected static function upsert(string $id, string $kind, array $payload, int $baseVersion = 0): array
    {
        return ['op' => 'upsert', 'id' => $id, 'kind' => $kind, 'baseVersion' => $baseVersion, 'payload' => $payload];
    }

    /** @return array<string, mixed> */
    protected static function delete(string $id, int $baseVersion): array
    {
        return ['op' => 'delete', 'id' => $id, 'baseVersion' => $baseVersion];
    }

    protected static function newId(): string
    {
        return Uuid::v7()->toRfc4122();
    }

    /**
     * A connection profile as the desktop app serializes one, without secrets.
     *
     * @param array<string, mixed> $overrides
     *
     * @return array<string, mixed>
     */
    protected static function connectionProfile(string $id, array $overrides = []): array
    {
        return [
            'id' => $id,
            'workspaceId' => 'personal',
            'folderId' => null,
            'name' => 'Shop production',
            'color' => 'red',
            'sortOrder' => 0,
            'engine' => 'postgres',
            'host' => 'db.shop.example.test',
            'port' => 5432,
            'database' => 'shop',
            'username' => 'kim',
            'environment' => 'production',
            'tls' => ['mode' => 'verify_full', 'caPath' => null, 'clientCertPath' => null, 'clientKeyPath' => null],
            'ssh' => ['enabled' => false, 'host' => '', 'port' => 22, 'username' => '', 'auth' => 'password', 'keyPath' => null],
            'options' => ['readOnly' => true, 'confirmDestructive' => true, 'statementTimeoutMs' => 30000, 'keepaliveS' => null, 'initSql' => null, 'showSystemObjects' => false, 'redisDb' => 0],
            'savedSecrets' => ['db_password', 'ssh_key_passphrase'],
            'createdAt' => 1762945200000,
            'updatedAt' => 1762945200000,
            'version' => 1,
            ...$overrides,
        ];
    }

    /**
     * @param array<string, mixed> $overrides
     *
     * @return array<string, mixed>
     */
    protected static function folderRecord(string $id, array $overrides = []): array
    {
        return ['id' => $id, 'workspaceId' => 'personal', 'parentId' => null, 'name' => 'Webshop', 'color' => null, 'sortOrder' => 1, ...$overrides];
    }

    /**
     * @param array<string, mixed> $overrides
     *
     * @return array<string, mixed>
     */
    protected static function savedQuery(string $id, array $overrides = []): array
    {
        return [
            'id' => $id,
            'workspaceId' => 'personal',
            'folderId' => null,
            'connectionId' => null,
            'database' => 'shop',
            'name' => 'Open orders',
            'sql' => "SELECT * FROM orders WHERE status = 'open'",
            'tags' => ['orders'],
            'sortOrder' => 2,
            ...$overrides,
        ];
    }
}
