<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Sync\Change;
use App\Sync\ChangeParser;
use App\Sync\PurgeSyncTombstones;
use App\Sync\PurgeSyncTombstonesHandler;
use App\Sync\RecordPull;
use App\Sync\RecordPush;
use App\Sync\Rejection;
use App\Sync\SyncScope;
use Doctrine\DBAL\Connection;
use Doctrine\DBAL\DriverManager;
use Doctrine\DBAL\Exception as DBALException;
use Symfony\Component\Clock\MockClock;
use Symfony\Component\Clock\Test\ClockSensitiveTrait;
use Symfony\Component\Uid\Uuid;

/**
 * How pushes to one scope take turns, the tombstone purge with the cursors it outdates, and the limit of live records.
 */
final class SyncStorageTest extends DatabaseTestCase
{
    use AuthenticatesUsers;
    use ClockSensitiveTrait;
    use SyncsRecords;

    /**
     * Two writers on connections of their own, outside the test transaction, because the lock must be visible to
     * another session: the second waits for the first, then sees its change, so a change can never be lost.
     */
    public function testConcurrentPushesTakeTurnsAndNeitherIsLost(): void
    {
        $first = DriverManager::getConnection($this->separateConnectionParams());
        $second = DriverManager::getConnection($this->separateConnectionParams());
        [$user, $organization] = $this->committedOwner($first);
        $scope = SyncScope::organization($organization);
        $id = self::newId();

        try {
            $first->beginTransaction();
            $results = $this->pushVia($first, $scope, [self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 1']))], $user);
            self::assertSame([['id' => $id, 'status' => 'accepted', 'version' => 1]], $results);

            $second->executeStatement("SET lock_timeout = '200ms'");
            try {
                $this->pushVia($second, $scope, [self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 2']))], $user);
                self::fail('A second writer went ahead while the first push was still open.');
            } catch (DBALException $exception) {
                self::assertStringContainsString('lock', strtolower($exception->getMessage()));
            }
            self::assertSame([], new RecordPull($second)->pull($scope, null, 10)['changes'], 'A pull never sees a push before it commits.');

            $first->commit();

            $retry = $this->pushVia($second, $scope, [self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 2']))], $user);
            self::assertSame('conflict', $retry[0]['status'] ?? null, 'The same create after the first committed is a conflict.');
            $update = $this->pushVia($second, $scope, [self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 2']), 1)], $user);
            self::assertSame([['id' => $id, 'status' => 'accepted', 'version' => 2]], $update);

            self::assertSame(['version' => 2, 'change_seq' => 2], $second->fetchAssociative('SELECT version, change_seq FROM record WHERE organization_id = :o', ['o' => $organization->toRfc4122()]));
            self::assertSame(2, $second->fetchOne('SELECT record_seq FROM organization WHERE id = :o', ['o' => $organization->toRfc4122()]));
            self::assertSame(1, $second->fetchOne('SELECT COUNT(*) FROM record_revision WHERE organization_id = :o', ['o' => $organization->toRfc4122()]), 'The first writer\'s SQL is kept as a revision.');
        } finally {
            if ($first->isTransactionActive()) {
                $first->rollBack();
            }
            $first->delete('organization', ['id' => $organization->toRfc4122()]);
            $first->delete('app_user', ['id' => $user->toRfc4122()]);
            $first->close();
            $second->close();
        }
    }

    public function testPurgedTombstonesSendOlderCursorsBackToAFullPull(): void
    {
        $this->bootstrapOAuthClients();
        $this->createUser('kim@example.test', 'Kim Visser');
        $kim = $this->userHeaders('kim@example.test');
        $organization = $this->organizationOf($kim);
        $push = self::organizationPath($organization, 'push');
        $changes = self::organizationPath($organization, 'changes');
        $old = self::newId();
        $kept = self::newId();
        $this->push($push, [self::upsert($old, 'folder', self::folderRecord($old)), self::upsert($kept, 'folder', self::folderRecord($kept))], $kim);
        $this->push('/api/v1/sync/user/push', [self::upsert($old, 'preference', ['theme' => 'dark'])], $kim);
        $beforeDelete = $this->pull($changes, $kim)['cursor'];
        $userBeforeDelete = $this->pull('/api/v1/sync/user/changes', $kim)['cursor'];
        $this->push($push, [self::delete($old, 1)], $kim);
        $this->push('/api/v1/sync/user/push', [self::delete($old, 1)], $kim);
        $afterDelete = $this->pull($changes, $kim, $beforeDelete)['cursor'];

        $purge = static::getContainer()->get(PurgeSyncTombstonesHandler::class);
        self::mockTime('+89 days');
        self::assertSame(0, $purge(new PurgeSyncTombstones()), 'A tombstone is kept for 90 days.');
        $this->pull($changes, $kim, $beforeDelete);

        self::mockTime('+91 days');
        self::assertSame(2, $purge(new PurgeSyncTombstones()));
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM record WHERE deleted_at IS NOT NULL'));
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM user_record'));

        $this->client->request('GET', $changes.'?cursor='.$beforeDelete, server: $kim);
        self::assertResponseStatusCodeSame(410, 'This client never heard of the delete.');
        self::assertStringContainsString('Pull everything again without a cursor.', (string) $this->client->getResponse()->getContent());
        $this->client->request('GET', '/api/v1/sync/user/changes?cursor='.$userBeforeDelete, server: $kim);
        self::assertResponseStatusCodeSame(410);

        self::assertSame([], $this->pull($changes, $kim, $afterDelete)['changes'], 'A client that had the delete goes on.');
        $full = $this->pull($changes, $kim);
        self::assertSame([$kept], array_column($full['changes'], 'id'));
        self::assertSame([], $this->pull($changes, $kim, $full['cursor'])['changes']);
    }

    public function testAScopeHoldsAtMostItsLimitOfLiveRecords(): void
    {
        $this->bootstrapOAuthClients();
        $this->createUser('kim@example.test', 'Kim Visser');
        $kim = $this->userHeaders('kim@example.test');
        $scope = SyncScope::organization(Uuid::fromString($this->organizationOf($kim)));
        $user = Uuid::fromString(self::string($this->connection()->fetchOne("SELECT id FROM app_user WHERE email = 'kim@example.test'")));
        $push = new RecordPush($this->connection(), new MockClock(), 2, 2);
        [$a, $b, $c] = [self::newId(), self::newId(), self::newId()];

        $results = $push->push($scope, $this->parse($scope, [
            self::upsert($a, 'folder', self::folderRecord($a)),
            self::upsert($b, 'folder', self::folderRecord($b)),
            self::upsert($c, 'folder', self::folderRecord($c)),
            self::upsert($a, 'folder', self::folderRecord($a, ['name' => 'Updates still go']), 1),
        ]), $user, null);
        self::assertSame(['accepted', 'accepted', 'rejected', 'accepted'], array_column($results, 'status'));
        self::assertSame(['id' => $c, 'status' => 'rejected', 'reason' => 'record_limit', 'field' => null, 'message' => 'There can be at most 2 records; delete some first.'], $results[2]);

        $results = $push->push($scope, $this->parse($scope, [
            self::delete($b, 1),
            self::upsert($c, 'folder', self::folderRecord($c)),
            self::upsert($b, 'folder', self::folderRecord($b), 2),
        ]), $user, null);
        self::assertSame(['accepted', 'accepted', 'rejected'], array_column($results, 'status'), 'A delete makes room; bringing a record back counts again.');
        self::assertSame(2, $this->countRows('SELECT COUNT(*) FROM record WHERE deleted_at IS NULL'));
    }

    /**
     * A user and their personal organization, committed, so other sessions see them.
     *
     * @return array{Uuid, Uuid}
     */
    private function committedOwner(Connection $connection): array
    {
        $user = Uuid::v7();
        $organization = Uuid::v7();
        $connection->insert('app_user', [
            'id' => $user->toRfc4122(), 'email' => 'lock-'.$user->toRfc4122().'@example.test', 'password' => 'x',
            'display_name' => 'Lock test', 'roles' => '[]', 'created_at' => '2026-11-12 00:00:00',
        ]);
        $connection->insert('organization', ['id' => $organization->toRfc4122(), 'personal_owner_id' => $user->toRfc4122(), 'created_at' => '2026-11-12 00:00:00']);

        return [$user, $organization];
    }

    /**
     * @param list<array<string, mixed>> $changes
     *
     * @return list<array<string, mixed>>
     */
    private function pushVia(Connection $connection, SyncScope $scope, array $changes, Uuid $user): array
    {
        return new RecordPush($connection, new MockClock(), 100, 100)->push($scope, $this->parse($scope, $changes), $user, null);
    }

    /**
     * @param list<array<string, mixed>> $changes
     *
     * @return list<Change|Rejection>
     */
    private function parse(SyncScope $scope, array $changes): array
    {
        return ChangeParser::parse(json_encode(['changes' => $changes], \JSON_THROW_ON_ERROR), $scope);
    }

    /**
     * @return array{driver: 'pdo_pgsql', host: string, port: int, user: string, password: string, dbname: string}
     */
    private function separateConnectionParams(): array
    {
        $params = $this->connection()->getParams();
        $host = $params['host'] ?? null;
        $user = $params['user'] ?? null;
        $password = $params['password'] ?? null;
        $dbname = $params['dbname'] ?? null;
        $port = $params['port'] ?? 5432;
        self::assertIsString($host);
        self::assertIsString($user);
        self::assertIsString($password);
        self::assertIsString($dbname);
        self::assertIsNumeric($port);

        return ['driver' => 'pdo_pgsql', 'host' => $host, 'port' => (int) $port, 'user' => $user, 'password' => $password, 'dbname' => $dbname];
    }
}
