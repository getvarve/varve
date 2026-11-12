<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Account\UserDeletion;
use App\Enum\RecordKind;
use App\Sync\Change;
use App\Sync\ChangeParser;
use App\Sync\Cursor;
use App\Sync\CursorExpired;
use App\Sync\PurgeSyncTombstones;
use App\Sync\PurgeSyncTombstonesHandler;
use App\Sync\RecordPull;
use App\Sync\RecordPush;
use App\Sync\Rejection;
use App\Sync\SyncScope;
use Doctrine\DBAL\Configuration;
use Doctrine\DBAL\Connection;
use Doctrine\DBAL\DriverManager;
use Doctrine\DBAL\Exception as DBALException;
use Doctrine\DBAL\Logging\Middleware as LoggingMiddleware;
use Psr\Log\AbstractLogger;
use Symfony\Component\Clock\MockClock;
use Symfony\Component\Clock\Test\ClockSensitiveTrait;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;
use Symfony\Component\Uid\Uuid;

/**
 * How pushes to one scope take turns, the tombstone purge with the cursors it outdates, how a pull and an account
 * deletion hold up against writers at the same moment, and the limits of what a scope stores.
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
        $push = $this->recordPush($this->connection(), records: 2);
        [$a, $b, $c] = [self::newId(), self::newId(), self::newId()];

        $results = $push->push($scope, $this->parse($scope, [
            self::upsert($a, 'folder', self::folderRecord($a)),
            self::upsert($b, 'folder', self::folderRecord($b)),
            self::upsert($c, 'folder', self::folderRecord($c)),
            self::upsert($a, 'folder', self::folderRecord($a, ['name' => 'Updates still go']), 1),
        ]), $user, null);
        self::assertSame(['accepted', 'accepted', 'rejected', 'accepted'], array_column($results, 'status'));
        self::assertSame(['id' => $c, 'status' => 'rejected', 'reason' => 'record_limit', 'field' => null, 'message' => 'There can be at most 2 records; delete some first.', 'limit' => 2], $results[2]);

        $results = $push->push($scope, $this->parse($scope, [
            self::delete($b, 1),
            self::upsert($c, 'folder', self::folderRecord($c)),
            self::upsert($b, 'folder', self::folderRecord($b), 2),
        ]), $user, null);
        self::assertSame(['accepted', 'accepted', 'rejected'], array_column($results, 'status'), 'A delete makes room; bringing a record back counts again.');
        self::assertSame(2, $this->countRows('SELECT COUNT(*) FROM record WHERE deleted_at IS NULL'));
    }

    public function testAScopeHoldsAtMostItsLimitOfBytes(): void
    {
        [$scope, $user] = $this->kimsOrganization();
        [$a, $b, $c] = [self::newId(), self::newId(), self::newId()];
        $size = $this->storedSize($scope, self::upsert($a, 'folder', self::folderRecord($a)));
        // Room for two folders and ten bytes.
        $limit = 2 * $size + 10;
        $push = $this->recordPush($this->connection(), bytes: $limit);

        $results = $push->push($scope, $this->parse($scope, [
            self::upsert($a, 'folder', self::folderRecord($a)),
            self::upsert($b, 'folder', self::folderRecord($b)),
            self::upsert($c, 'folder', self::folderRecord($c)),
            self::upsert($a, 'folder', self::folderRecord($a, ['name' => 'Webshop, grown by more than ten bytes']), 1),
            self::upsert($a, 'folder', self::folderRecord($a, ['name' => 'Webshop'.str_repeat('+', 10)]), 1),
            self::upsert($a, 'folder', self::folderRecord($a, ['name' => 'Web']), 2),
        ]), $user, null);

        self::assertSame(['accepted', 'accepted', 'rejected', 'rejected', 'accepted', 'accepted'], array_column($results, 'status'));
        $rejected = ['status' => 'rejected', 'reason' => 'storage_limit', 'field' => null, 'message' => \sprintf('The records can hold at most %d bytes together; make some smaller or delete some first.', $limit), 'limit' => $limit];
        self::assertSame(['id' => $c, ...$rejected], $results[2], 'A third folder does not fit.');
        self::assertSame(['id' => $a, ...$rejected], $results[3], 'Nor does a record that grows past the limit.');
        self::assertSame(2 * $size - 4, $this->countRows('SELECT SUM(payload_bytes) FROM record'), 'Growing up to the limit and shrinking both went.');

        $results = $push->push($scope, $this->parse($scope, [
            self::delete($b, 1),
            self::upsert($c, 'folder', self::folderRecord($c)),
            self::upsert($b, 'folder', self::folderRecord($b), 2),
        ]), $user, null);
        self::assertSame(['accepted', 'accepted', 'rejected'], array_column($results, 'status'), 'A delete makes room; bringing a record back counts again.');
        self::assertSame('storage_limit', $results[2]['reason'] ?? null);

        // A server that lowers its limit: what is stored stays and can still shrink or go, but not grow.
        $lowered = $this->recordPush($this->connection(), bytes: 1);
        $results = $lowered->push($scope, $this->parse($scope, [
            self::upsert($a, 'folder', self::folderRecord($a, ['name' => 'We']), 3),
            self::upsert($a, 'folder', self::folderRecord($a, ['name' => 'Us']), 4),
            self::upsert($a, 'folder', self::folderRecord($a, ['name' => 'Web']), 5),
            self::delete($c, 1),
        ]), $user, null);
        self::assertSame(['accepted', 'accepted', 'rejected', 'accepted'], array_column($results, 'status'));
        self::assertSame($this->countRows('SELECT SUM(octet_length(payload::text)) FROM record'), $this->countRows('SELECT SUM(payload_bytes) FROM record'), 'The sizes kept are the sizes stored.');
        self::assertSame(0, $this->countRows('SELECT SUM(payload_bytes) FROM record WHERE deleted_at IS NOT NULL'), 'A tombstone takes no room.');
    }

    public function testAUsersOwnRecordsHaveTheirOwnLimitOfBytes(): void
    {
        [, $user] = $this->kimsOrganization();
        $scope = SyncScope::user($user);
        [$a, $b] = [self::newId(), self::newId()];
        $size = $this->storedSize($scope, self::upsert($a, 'preference', ['theme' => 'dark']));
        // The organization's limit is far away; the user's leaves room for one record.
        $push = new RecordPush($this->connection(), new MockClock(), 100, 100, 100_000_000, $size + 1, 50, 100_000_000);

        $results = $push->push($scope, $this->parse($scope, [self::upsert($a, 'preference', ['theme' => 'dark']), self::upsert($b, 'preference', ['theme' => 'dark'])]), $user, null);

        self::assertSame(['accepted', 'rejected'], array_column($results, 'status'));
        self::assertSame(['storage_limit', $size + 1], [$results[1]['reason'] ?? null, $results[1]['limit'] ?? null]);
        self::assertSame($size, $this->countRows('SELECT SUM(payload_bytes) FROM user_record'));
    }

    public function testRevisionsOfARecordAreCappedByNumberAndByBytes(): void
    {
        [$scope, $user] = $this->kimsOrganization();
        $id = self::newId();
        $versions = fn (): array => $this->connection()->fetchFirstColumn('SELECT version FROM record_revision WHERE record_id = :id ORDER BY version DESC', ['id' => $id]);
        $updates = static function (int $from, int $to) use ($id): array {
            $changes = [];
            for ($version = $from; $version <= $to; ++$version) {
                $changes[] = self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT '.$version]), $version);
            }

            return $changes;
        };
        $size = $this->storedSize($scope, self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 1'])));

        $this->recordPush($this->connection(), revisions: 3)->push($scope, $this->parse($scope, [self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 0'])), ...$updates(1, 6)]), $user, null);
        self::assertSame([6, 5, 4], $versions(), 'The newest three.');
        self::assertSame(3 * $size, $this->countRows('SELECT SUM(payload_bytes) FROM record_revision'), 'A revision keeps the size of its payload.');

        // Room for two revisions and a half.
        $this->recordPush($this->connection(), revisionBytes: (int) (2.5 * $size))->push($scope, $this->parse($scope, $updates(7, 7)), $user, null);
        self::assertSame([7, 6], $versions(), 'The newest that fit in the bytes.');

        $this->recordPush($this->connection(), revisionBytes: 1)->push($scope, $this->parse($scope, $updates(8, 8)), $user, null);
        self::assertSame([8], $versions(), 'The newest revision always stays.');
    }

    /**
     * The tombstone purge commits at each moment it could during a pull of a client that has not had the delete yet:
     * before each statement the pull sends. Whenever it lands, the client either gets the tombstone or is told to pull
     * everything again; it is never answered as if nothing was deleted. On connections of their own, since the purge
     * must commit while the pull runs.
     */
    public function testAPullNeverMissesADeleteThePurgeTakesMeanwhile(): void
    {
        $writer = DriverManager::getConnection($this->separateConnectionParams());
        $purger = DriverManager::getConnection($this->separateConnectionParams());
        $purge = new PurgeSyncTombstonesHandler($purger, new MockClock('+91 days'));
        $statements = new class extends AbstractLogger {
            public int $count = 0;
            public int $runAt = 0;
            public ?\Closure $run = null;

            public function log(mixed $level, string|\Stringable $message, array $context = []): void
            {
                if (str_starts_with((string) $message, 'Executing') && ++$this->count === $this->runAt && null !== $this->run) {
                    ($this->run)();
                }
            }
        };
        $reader = DriverManager::getConnection($this->separateConnectionParams(), new Configuration()->setMiddlewares([new LoggingMiddleware($statements)]));
        $reader->executeQuery('SELECT 1');
        $owners = [];

        try {
            foreach ([1, 2, 3] as $moment) {
                $owners[] = [$user, $organization] = $this->committedOwner($writer);
                $scope = SyncScope::organization($organization);
                [$kept, $gone] = [self::newId(), self::newId()];
                $this->pushVia($writer, $scope, [self::upsert($kept, 'folder', self::folderRecord($kept)), self::upsert($gone, 'folder', self::folderRecord($gone))], $user);
                $cursor = Cursor::decode(new RecordPull($writer)->pull($scope, null, 10)['cursor'], $scope);
                self::assertNotNull($cursor);
                $this->pushVia($writer, $scope, [self::delete($gone, 1)], $user);

                $purged = 0;
                $statements->count = 0;
                $statements->runAt = $moment;
                $statements->run = static function () use ($purge, &$purged): void {
                    $purged = $purge(new PurgeSyncTombstones());
                };
                try {
                    $page = new RecordPull($reader)->pull($scope, $cursor, 10);
                    self::assertSame([[$gone, true]], array_map(static fn (array $record): array => [$record['id'], $record['deleted']], $page['changes']), \sprintf('A purge before statement %d: the pull was answered, so it must hold the delete.', $moment));
                } catch (CursorExpired) {
                    self::assertGreaterThanOrEqual(1, $purged, 'Only a purge sends a cursor back to a full pull.');
                }
                if (1 === $moment) {
                    self::assertGreaterThanOrEqual(1, $purged, 'The purge ran before the first statement of the pull.');
                }
                self::assertGreaterThanOrEqual(1, $statements->count);
            }
        } finally {
            $statements->run = null;
            foreach ($owners as [$user, $organization]) {
                $writer->delete('organization', ['id' => $organization->toRfc4122()]);
                $writer->delete('app_user', ['id' => $user->toRfc4122()]);
            }
            $writer->close();
            $purger->close();
            $reader->close();
        }
    }

    /**
     * An account deletion while a push to one of the account's feeds is open: the deletion takes the locks a push
     * takes before it deletes anything, the organization's first, so it waits for the push instead of deadlocking
     * with it. A push after the deletion finds its owner gone.
     */
    public function testDeletingAnAccountWaitsForAnOpenPushAndNeverMeetsItHalfway(): void
    {
        $pushing = DriverManager::getConnection($this->separateConnectionParams());
        $statements = new class extends AbstractLogger {
            /** @var list<string> */
            public array $sql = [];

            public function log(mixed $level, string|\Stringable $message, array $context = []): void
            {
                if (str_starts_with((string) $message, 'Executing') && \is_string($context['sql'] ?? null)) {
                    $this->sql[] = $context['sql'];
                }
            }

            public function forget(): void
            {
                $this->sql = [];
            }
        };
        $deleting = DriverManager::getConnection($this->separateConnectionParams(), new Configuration()->setMiddlewares([new LoggingMiddleware($statements)]));
        $deleting->executeStatement("SET lock_timeout = '200ms'");
        $deletion = new UserDeletion($deleting);
        $owners = [];

        try {
            foreach (['organization', 'user'] as $feed) {
                $owners[] = [$user, $organization] = $this->committedOwner($pushing);
                $scope = 'organization' === $feed ? SyncScope::organization($organization) : SyncScope::user($user);
                $kind = 'organization' === $feed ? 'folder' : 'preference';
                [$first, $second] = [self::newId(), self::newId()];

                $pushing->beginTransaction();
                $this->pushVia($pushing, $scope, [self::upsert($first, $kind, ['name' => 'First'])], $user);
                try {
                    $deletion->delete($user);
                    self::fail('The deletion went ahead while a push to the '.$feed.' feed was open.');
                } catch (DBALException $exception) {
                    self::assertStringContainsString('lock', strtolower($exception->getMessage()));
                }
                // The deletion holds nothing the push needs: the push writes on and commits.
                self::assertSame('accepted', $this->pushVia($pushing, $scope, [self::upsert($second, $kind, ['name' => 'Second'])], $user)[0]['status'] ?? null);
                $pushing->commit();
                self::assertSame(2, $pushing->fetchOne(\sprintf('SELECT COUNT(*) FROM %s WHERE %s = :owner', $scope->table, $scope->ownerColumn), ['owner' => $scope->owner->toRfc4122()]));

                $statements->forget();
                $deletion->delete($user);

                $locksAndDeletes = array_values(array_filter($statements->sql, static fn (string $sql): bool => str_contains($sql, 'FOR UPDATE') || str_starts_with($sql, 'DELETE')));
                self::assertStringStartsWith('SELECT 1 FROM organization WHERE personal_owner_id', $locksAndDeletes[0] ?? '', 'The organization is locked first.');
                self::assertStringStartsWith('SELECT 1 FROM app_user WHERE id', $locksAndDeletes[1] ?? '', 'Then the user, and only then anything is deleted.');
                self::assertGreaterThan(2, \count($locksAndDeletes));
                foreach (['app_user' => $user, 'organization' => $organization] as $table => $id) {
                    self::assertSame(0, $pushing->fetchOne(\sprintf('SELECT COUNT(*) FROM %s WHERE id = :id', $table), ['id' => $id->toRfc4122()]));
                }
                self::assertSame(0, $pushing->fetchOne(\sprintf('SELECT COUNT(*) FROM %s WHERE %s = :owner', $scope->table, $scope->ownerColumn), ['owner' => $scope->owner->toRfc4122()]));

                try {
                    $this->pushVia($pushing, $scope, [self::upsert(self::newId(), $kind, ['name' => 'Too late'])], $user);
                    self::fail('A push to a deleted owner was answered.');
                } catch (NotFoundHttpException $exception) {
                    self::assertSame('organization' === $feed ? 'Organization not found.' : 'User not found.', $exception->getMessage());
                }
                try {
                    new RecordPull($pushing)->pull($scope, null, 10);
                    self::fail('A pull from a deleted owner was answered.');
                } catch (NotFoundHttpException $exception) {
                    self::assertSame('organization' === $feed ? 'Organization not found.' : 'User not found.', $exception->getMessage());
                }
            }
        } finally {
            if ($pushing->isTransactionActive()) {
                $pushing->rollBack();
            }
            foreach ($owners as [$user, $organization]) {
                $pushing->delete('organization', ['id' => $organization->toRfc4122()]);
                $pushing->delete('app_user', ['id' => $user->toRfc4122()]);
            }
            $pushing->close();
            $deleting->close();
        }
    }

    /**
     * A statement that fails in the middle of a push (here a payload the database refuses, which the parser never
     * lets through) takes the whole push with it: no change of it is stored and the feed's number stays.
     */
    public function testAFailingStatementLeavesNothingOfThePush(): void
    {
        [$scope, $user] = $this->kimsOrganization();
        [$a, $b] = [self::newId(), self::newId()];
        $this->pushVia($this->connection(), $scope, [self::upsert($a, 'query', self::savedQuery($a))], $user);

        try {
            $this->recordPush($this->connection())->push($scope, [
                ...$this->parse($scope, [self::upsert($a, 'query', self::savedQuery($a, ['sql' => 'SELECT 2']), 1), self::upsert($b, 'folder', self::folderRecord($b))]),
                Change::upsert(self::newId(), 0, RecordKind::Folder, '{"name": not JSON'),
            ], $user, null);
            self::fail('The database took a payload that is not JSON.');
        } catch (DBALException) {
        }

        self::assertSame([['id' => $a, 'version' => 1, 'change_seq' => 1]], $this->connection()->fetchAllAssociative('SELECT id, version, change_seq FROM record'));
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM record_revision'));
        self::assertSame(1, $this->countRows('SELECT record_seq FROM organization WHERE id = :id', ['id' => $scope->owner->toRfc4122()]));
    }

    public function testAPullKeepsItsPageSizeWithinBoundsWhateverItIsAsked(): void
    {
        [$scope, $user] = $this->kimsOrganization();
        $changes = [];
        for ($i = 0; $i < 3; ++$i) {
            $id = self::newId();
            $changes[] = self::upsert($id, 'folder', self::folderRecord($id));
        }
        $this->pushVia($this->connection(), $scope, $changes, $user);
        $pull = new RecordPull($this->connection());

        $page = $pull->pull($scope, null, 0);
        self::assertCount(1, $page['changes'], 'A page always holds a record, so paging always moves on.');
        self::assertTrue($page['hasMore']);
        self::assertCount(1, $pull->pull($scope, null, -5)['changes']);
        self::assertCount(3, $pull->pull($scope, null, 1_000_000)['changes']);
    }

    /**
     * Kim's personal organization as a scope, and Kim's id.
     *
     * @return array{SyncScope, Uuid}
     */
    private function kimsOrganization(): array
    {
        $this->bootstrapOAuthClients();
        $this->createUser('kim@example.test', 'Kim Visser');
        $kim = $this->userHeaders('kim@example.test');

        return [
            SyncScope::organization(Uuid::fromString($this->organizationOf($kim))),
            Uuid::fromString(self::string($this->connection()->fetchOne("SELECT id FROM app_user WHERE email = 'kim@example.test'"))),
        ];
    }

    /**
     * The bytes an upsert's payload takes as the server stores it.
     *
     * @param array<string, mixed> $upsert
     */
    private function storedSize(SyncScope $scope, array $upsert): int
    {
        $change = $this->parse($scope, [$upsert])[0];
        self::assertInstanceOf(Change::class, $change);

        return \strlen((string) $change->payload);
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
        return $this->recordPush($connection)->push($scope, $this->parse($scope, $changes), $user, null);
    }

    /**
     * A push with these limits; without any, limits no test reaches.
     */
    private function recordPush(Connection $connection, int $records = 100, int $bytes = 100_000_000, int $revisions = 50, int $revisionBytes = 100_000_000, ?MockClock $clock = null): RecordPush
    {
        return new RecordPush($connection, $clock ?? new MockClock(), $records, $records, $bytes, $bytes, $revisions, $revisionBytes);
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
