<?php

declare(strict_types=1);

namespace App\Sync;

use Doctrine\DBAL\Connection;
use Psr\Clock\ClockInterface;
use Symfony\Component\Messenger\Attribute\AsMessageHandler;
use Symfony\Component\Uid\Uuid;

/**
 * A tombstone tells clients that a record is gone; after 90 days every device that still syncs has had it, and the row
 * goes (with the revisions of a deleted saved query). Each owner's tombstones go in a transaction of their own that
 * holds the owner's row lock, as a push does, so a push never sees a record vanish under it. The owner's
 * record_purge_seq then rises to the last purged change number: a cursor before it has missed a delete, and its client
 * is told to pull everything again ({@see CursorExpired}).
 */
#[AsMessageHandler]
final readonly class PurgeSyncTombstonesHandler
{
    public const string KEEP_TOMBSTONES = 'P90D';

    public function __construct(
        private Connection $connection,
        private ClockInterface $clock,
    ) {
    }

    /** @return int how many tombstones were deleted */
    public function __invoke(PurgeSyncTombstones $message): int
    {
        $before = $this->clock->now()->sub(new \DateInterval(self::KEEP_TOMBSTONES))->format('Y-m-d H:i:s');
        $deleted = 0;
        foreach (['record' => ['organization_id', SyncScope::organization(...)], 'user_record' => ['user_id', SyncScope::user(...)]] as $table => [$ownerColumn, $scopeOf]) {
            $owners = $this->connection->fetchFirstColumn(
                \sprintf('SELECT DISTINCT %s FROM %s WHERE deleted_at < :before', $ownerColumn, $table),
                ['before' => $before],
            );
            foreach ($owners as $owner) {
                if (\is_string($owner)) {
                    $deleted += $this->purge($scopeOf(Uuid::fromString($owner)), $before);
                }
            }
        }

        return $deleted;
    }

    private function purge(SyncScope $scope, string $before): int
    {
        return $this->connection->transactional(function () use ($scope, $before): int {
            $owner = $scope->owner->toRfc4122();
            $this->connection->executeStatement(\sprintf('SELECT 1 FROM %s WHERE id = :owner FOR NO KEY UPDATE', $scope->ownerTable), ['owner' => $owner]);
            $purged = $this->connection->fetchAssociative(
                \sprintf(
                    'WITH purged AS (DELETE FROM %s WHERE %s = :owner AND deleted_at < :before RETURNING change_seq)
                     SELECT COUNT(*) AS count, MAX(change_seq) AS last FROM purged',
                    $scope->table, $scope->ownerColumn,
                ),
                ['owner' => $owner, 'before' => $before],
            );
            if (false === $purged || null === $purged['last']) {
                return 0;
            }
            $this->connection->executeStatement(
                \sprintf('UPDATE %s SET record_purge_seq = GREATEST(record_purge_seq, :last) WHERE id = :owner', $scope->ownerTable),
                ['last' => RecordRows::int($purged['last']), 'owner' => $owner],
            );

            return RecordRows::int($purged['count']);
        });
    }
}
