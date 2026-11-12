<?php

declare(strict_types=1);

namespace App\Sync;

use Doctrine\DBAL\Connection;

/**
 * Reads a scope's changes after a cursor, in change number order and in pages (design 8.8).
 *
 * Every record appears once, as it is now, at the change number of its last write; tombstones included. A first pull
 * (no cursor) gets the live records only, paged like any other pull (see {@see Cursor::$full}).
 *
 * The feed's number is read first and the rows up to it after: numbers are handed out under the owner's row lock and
 * committed with it, so every row up to that number is committed and visible by then. A write between two pages moves
 * its record to a higher number, which a later page or the next pull brings.
 *
 * A page holds at most `limit` records and stops once its payloads pass {@see self::PAGE_BYTES}, but always holds at
 * least one record.
 */
final readonly class RecordPull
{
    public const int DEFAULT_LIMIT = 200;
    public const int MAX_LIMIT = 1000;
    /** 4 MiB of payload per page, give or take the last record. */
    public const int PAGE_BYTES = 4194304;

    public function __construct(private Connection $connection)
    {
    }

    /**
     * @return array{changes: list<array<string, mixed>>, cursor: string, hasMore: bool}
     *
     * @throws CursorExpired when the cursor is from before the last tombstone purge, or from a later state of the feed
     */
    public function pull(SyncScope $scope, ?Cursor $cursor, int $limit): array
    {
        $owner = $this->connection->fetchAssociative(
            \sprintf('SELECT record_seq, record_purge_seq FROM %s WHERE id = :owner', $scope->ownerTable),
            ['owner' => $scope->owner->toRfc4122()],
        );
        if (false === $owner) {
            throw new \LogicException('The owner of this scope does not exist.');
        }
        $until = RecordRows::int($owner['record_seq']);
        if (null !== $cursor && ($cursor->seq > $until || RecordRows::int($owner['record_purge_seq']) > $cursor->horizon())) {
            throw new CursorExpired();
        }
        $cursor ??= new Cursor(0, $until);

        $range = ['owner' => $scope->owner->toRfc4122(), 'after' => $cursor->seq, 'until' => $until, 'full' => $cursor->full];
        $where = \sprintf(
            'r.%s = :owner AND r.change_seq > :after AND r.change_seq <= :until AND (r.deleted_at IS NULL OR r.change_seq > :full)',
            $scope->ownerColumn,
        );

        /** @var list<array{change_seq: int|string, size: int|string}> $sizes */
        $sizes = $this->connection->fetchAllAssociative(
            \sprintf('SELECT r.change_seq, COALESCE(octet_length(r.payload::text), 0) AS size FROM %s r WHERE %s ORDER BY r.change_seq LIMIT :rows', $scope->table, $where),
            $range + ['rows' => $limit + 1],
        );
        $last = $cursor->seq;
        $bytes = 0;
        $hasMore = false;
        foreach ($sizes as $index => $row) {
            $bytes += RecordRows::int($row['size']);
            if ($index === $limit || ($index > 0 && $bytes > self::PAGE_BYTES)) {
                $hasMore = true;
                break;
            }
            $last = RecordRows::int($row['change_seq']);
        }

        $rows = $sizes === [] ? [] : $this->connection->fetchAllAssociative(
            \sprintf('SELECT %s FROM %s r WHERE %s AND r.change_seq <= :last ORDER BY r.change_seq', RecordRows::columns($scope), $scope->table, $where),
            $range + ['last' => $last],
        );

        return [
            'changes' => array_map(static fn (array $row): array => RecordRows::toArray($row, $scope), $rows),
            'cursor' => new Cursor($hasMore ? $last : $until, $cursor->full)->encode($scope),
            'hasMore' => $hasMore,
        ];
    }
}
