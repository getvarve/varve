<?php

declare(strict_types=1);

namespace App\Sync;

use Doctrine\DBAL\Connection;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;

/**
 * Reads a scope's changes after a cursor, in change number order and in pages (design 8.8).
 *
 * Every record appears once, as it is now, at the change number of its last write; tombstones included. A first pull
 * (no cursor) gets the live records only, paged like any other pull (see {@see Cursor::$full}).
 *
 * A pull is one statement, so it reads one state of the database: the feed's number, the number of the last purged
 * tombstone and the rows all belong together. Numbers are handed out under the owner's row lock and committed with it,
 * so every row up to the feed's number is in that state; and a tombstone purge is in it either with the rows it removed
 * and the purge number it raised, or not at all. A cursor from before a purge can therefore never get an answer that
 * misses the purged delete: it gets {@see CursorExpired}. A write between two pages moves its record to a higher
 * number, which a later page or the next pull brings.
 *
 * A page holds at most `limit` records and stops once its payloads pass {@see self::PAGE_BYTES}, but always holds at
 * least one record. The sizes come from the payload_bytes column, so only the payloads of the page itself are read.
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
     * @throws CursorExpired         when the cursor is from before the last tombstone purge, or from a later state of the feed
     * @throws NotFoundHttpException when the owner of the scope is gone (deleted while the request was on its way)
     */
    public function pull(SyncScope $scope, ?Cursor $cursor, int $limit): array
    {
        $limit = max(1, min(self::MAX_LIMIT, $limit));
        // `feed` is the owner's row; `candidates` are the next records in the feed, one more than a page may hold, each
        // with its place and the bytes of the page up to it; the outer query reads the payloads of the page only.
        $rows = $this->connection->fetchAllAssociative(
            \sprintf(
                'WITH feed AS (SELECT record_seq, record_purge_seq FROM %1$s WHERE id = :owner),
                 candidates AS (
                     SELECT r.change_seq, ROW_NUMBER() OVER (ORDER BY r.change_seq) AS position, SUM(r.payload_bytes) OVER (ORDER BY r.change_seq) AS bytes
                     FROM %2$s r, feed
                     WHERE r.%3$s = :owner AND r.change_seq > :after AND r.change_seq <= feed.record_seq
                       AND (r.deleted_at IS NULL OR r.change_seq > COALESCE(CAST(:full AS BIGINT), feed.record_seq))
                     ORDER BY r.change_seq LIMIT :candidates
                 )
                 SELECT feed.record_seq, feed.record_purge_seq, (SELECT COUNT(*) FROM candidates) AS found, page.*
                 FROM feed LEFT JOIN (
                     SELECT %4$s FROM candidates c JOIN %2$s r ON r.%3$s = :owner AND r.change_seq = c.change_seq
                     WHERE c.position <= :limit AND (c.position = 1 OR c.bytes <= :bytes)
                 ) page ON TRUE
                 ORDER BY page.change_seq',
                $scope->ownerTable, $scope->table, $scope->ownerColumn, RecordRows::columns($scope),
            ),
            [
                'owner' => $scope->owner->toRfc4122(),
                'after' => null === $cursor ? 0 : $cursor->seq,
                // A first pull leaves out the tombstones up to the number the feed is at now.
                'full' => $cursor?->full,
                'candidates' => $limit + 1,
                'limit' => $limit,
                'bytes' => self::PAGE_BYTES,
            ],
        );
        if ([] === $rows) {
            throw $scope->notFound();
        }
        $until = RecordRows::int($rows[0]['record_seq']);
        if (null !== $cursor && ($cursor->seq > $until || RecordRows::int($rows[0]['record_purge_seq']) > $cursor->horizon())) {
            throw new CursorExpired();
        }
        $cursor ??= new Cursor(0, $until);

        // Without a record to return there is still the one row of the feed, with nulls for the record.
        $page = null === $rows[0]['id'] ? [] : $rows;
        $hasMore = RecordRows::int($rows[0]['found']) > \count($page);
        $last = [] === $page ? $cursor->seq : RecordRows::int($page[array_key_last($page)]['change_seq']);

        return [
            'changes' => array_map(static fn (array $row): array => RecordRows::toArray($row, $scope), $page),
            'cursor' => new Cursor($hasMore ? $last : $until, $cursor->full)->encode($scope),
            'hasMore' => $hasMore,
        ];
    }
}
