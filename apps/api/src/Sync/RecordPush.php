<?php

declare(strict_types=1);

namespace App\Sync;

use App\Enum\RecordKind;
use Doctrine\DBAL\Connection;
use Psr\Clock\ClockInterface;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;
use Symfony\Component\Uid\Uuid;

/**
 * Applies the changes of a push to one scope, in their order, in one transaction (design 8.8).
 *
 * The push first locks the owner's row (FOR NO KEY UPDATE), which every write to the scope does, and the tombstone
 * purge too. So writers of one scope take turns: each sees every change committed before it, the optimistic check on
 * `baseVersion` can never pass for two writers of the same version, and change numbers are handed out under the lock
 * and committed in their order. A client that pulled up to number N can therefore never meet a smaller number later.
 * Different scopes never wait on each other.
 *
 * Per change:
 * - an upsert with baseVersion 0 creates the record; when the id is taken (a live record or a tombstone), that is a
 *   conflict, never an overwrite;
 * - an upsert or delete with the record's current version writes the next version; with any other version it is a
 *   conflict, answered with the server's copy (null when the server has no such record);
 * - a delete leaves a tombstone without payload; deleting a tombstone again with its version changes nothing;
 * - a change that creates or revives a record past the scope's limit of live records is rejected;
 * - a change that makes a record larger (or creates or revives one) is rejected when the payloads of the scope's live
 *   records would then pass the scope's limit of bytes. A change that makes a record smaller always goes.
 */
final readonly class RecordPush
{
    public function __construct(
        private Connection $connection,
        private ClockInterface $clock,
        #[Autowire('%env(int:VARVE_SYNC_MAX_RECORDS)%')]
        private int $maxRecords,
        #[Autowire('%env(int:VARVE_SYNC_MAX_USER_RECORDS)%')]
        private int $maxUserRecords,
        #[Autowire('%env(int:VARVE_SYNC_MAX_BYTES)%')]
        private int $maxBytes,
        #[Autowire('%env(int:VARVE_SYNC_MAX_USER_BYTES)%')]
        private int $maxUserBytes,
        #[Autowire('%env(int:VARVE_SYNC_MAX_REVISIONS)%')]
        private int $maxRevisions,
        #[Autowire('%env(int:VARVE_SYNC_MAX_REVISION_BYTES)%')]
        private int $maxRevisionBytes,
    ) {
    }

    /**
     * @param list<Change|Rejection> $changes
     *
     * @return list<array<string, mixed>> one result per change, in the same order
     *
     * @throws NotFoundHttpException when the owner of the scope is gone (deleted while the request was on its way)
     */
    public function push(SyncScope $scope, array $changes, Uuid $user, ?Uuid $device): array
    {
        return $this->connection->transactional(function () use ($scope, $changes, $user, $device): array {
            $locked = $this->connection->fetchOne(
                \sprintf('SELECT record_seq FROM %s WHERE id = :owner FOR NO KEY UPDATE', $scope->ownerTable),
                ['owner' => $scope->owner->toRfc4122()],
            );
            if (false === $locked) {
                throw $scope->notFound();
            }
            $seq = RecordRows::int($locked);
            /** @var array{records: int, bytes: int}|null $usage what the scope's live records add up to, once a change needs it */
            $usage = null;
            $maxRecords = $scope->isOrganization() ? $this->maxRecords : $this->maxUserRecords;
            $maxBytes = $scope->isOrganization() ? $this->maxBytes : $this->maxUserBytes;
            $now = $this->clock->now()->format('Y-m-d H:i:s');
            $writer = ['owner' => $scope->owner->toRfc4122(), 'now' => $now, 'device' => $device?->toRfc4122(), 'user' => $user->toRfc4122()];

            $results = [];
            foreach ($changes as $change) {
                if ($change instanceof Rejection) {
                    $results[] = $change->toArray();
                    continue;
                }

                $current = $this->current($scope, $change->id);
                $version = null === $current ? null : RecordRows::int($current['version']);
                $tombstone = null !== $current && null !== $current['deleted_at'];
                $bytesBefore = null === $current ? 0 : RecordRows::int($current['payload_bytes']);

                if ($change->isDelete()) {
                    if (null === $current) {
                        $results[] = new Rejection($change->id, Rejection::NOT_FOUND, 'There is no record with this id to delete.')->toArray();
                    } elseif ($version !== $change->baseVersion) {
                        $results[] = self::conflict($change, $current, $scope);
                    } elseif ($tombstone) {
                        $results[] = self::accepted($change, $version);
                    } else {
                        $this->keepRevision($scope, $change->id, $current);
                        $this->write($scope, 'payload = NULL, payload_bytes = 0, deleted_at = :now', $change, $writer + ['seq' => ++$seq]);
                        if (null !== $usage) {
                            $usage = ['records' => $usage['records'] - 1, 'bytes' => $usage['bytes'] - $bytesBefore];
                        }
                        $results[] = self::accepted($change, $version + 1);
                    }
                    continue;
                }

                if (null === $current ? 0 !== $change->baseVersion : $version !== $change->baseVersion) {
                    $results[] = self::conflict($change, $current, $scope);
                    continue;
                }
                if (null !== $current && $current['kind'] !== $change->kind?->value) {
                    $results[] = new Rejection($change->id, Rejection::INVALID, \sprintf('This record is a %s; its kind cannot change.', self::string($current['kind'])), 'kind')->toArray();
                    continue;
                }
                $bytes = \strlen((string) $change->payload);
                $adds = null === $current || $tombstone;
                if ($adds || $bytes > $bytesBefore) {
                    $usage ??= $this->usage($scope);
                    if ($adds && $usage['records'] >= $maxRecords) {
                        $results[] = new Rejection($change->id, Rejection::RECORD_LIMIT, \sprintf('There can be at most %d records; delete some first.', $maxRecords), limit: $maxRecords)->toArray();
                        continue;
                    }
                    if ($usage['bytes'] - $bytesBefore + $bytes > $maxBytes) {
                        $results[] = new Rejection($change->id, Rejection::STORAGE_LIMIT, \sprintf('The records can hold at most %d bytes together; make some smaller or delete some first.', $maxBytes), limit: $maxBytes)->toArray();
                        continue;
                    }
                }
                if (null !== $usage) {
                    $usage = ['records' => $usage['records'] + ($adds ? 1 : 0), 'bytes' => $usage['bytes'] - $bytesBefore + $bytes];
                }

                if (null === $current) {
                    $this->connection->executeStatement(
                        \sprintf(
                            'INSERT INTO %s (%s, id, kind, payload, payload_bytes, version, change_seq, created_at, updated_at, updated_device_id%s)
                             VALUES (:owner, :id, :kind, CAST(:payload AS JSON), :bytes, 1, :seq, :now, :now, :device%s)',
                            $scope->table, $scope->ownerColumn,
                            $scope->recordsAuthor ? ', created_by_id, updated_by_id' : '', $scope->recordsAuthor ? ', :user, :user' : '',
                        ),
                        self::only($writer + ['id' => $change->id, 'kind' => $change->kind?->value, 'payload' => $change->payload, 'bytes' => $bytes, 'seq' => ++$seq], $scope),
                    );
                    $results[] = self::accepted($change, 1);
                    continue;
                }

                if (!$tombstone) {
                    $this->keepRevision($scope, $change->id, $current);
                }
                $this->write($scope, 'payload = CAST(:payload AS JSON), payload_bytes = :bytes, deleted_at = NULL', $change, $writer + ['payload' => $change->payload, 'bytes' => $bytes, 'seq' => ++$seq]);
                $results[] = self::accepted($change, $version + 1);
            }

            $this->connection->executeStatement(
                \sprintf('UPDATE %s SET record_seq = :seq WHERE id = :owner', $scope->ownerTable),
                ['seq' => $seq, 'owner' => $scope->owner->toRfc4122()],
            );

            return $results;
        });
    }

    /**
     * The record as it is now, tombstones included.
     *
     * @return array<string, mixed>|null
     */
    private function current(SyncScope $scope, string $id): ?array
    {
        $row = $this->connection->fetchAssociative(
            \sprintf('SELECT %s FROM %s r WHERE r.%s = :owner AND r.id = :id', RecordRows::columns($scope), $scope->table, $scope->ownerColumn),
            ['owner' => $scope->owner->toRfc4122(), 'id' => $id],
        );

        return false === $row ? null : $row;
    }

    /**
     * Writes the next version of an existing record.
     *
     * @param array<string, mixed> $parameters
     */
    private function write(SyncScope $scope, string $set, Change $change, array $parameters): void
    {
        $this->connection->executeStatement(
            \sprintf(
                'UPDATE %s SET %s, version = version + 1, change_seq = :seq, updated_at = :now, updated_device_id = :device%s WHERE %s = :owner AND id = :id',
                $scope->table, $set, $scope->recordsAuthor ? ', updated_by_id = :user' : '', $scope->ownerColumn,
            ),
            self::only($parameters + ['id' => $change->id], $scope),
        );
    }

    /**
     * Keeps the payload a change to a saved query replaces, then drops the oldest revisions of the record past the
     * number and the bytes a record may keep. The newest revision always stays.
     *
     * @param array<string, mixed> $current
     */
    private function keepRevision(SyncScope $scope, string $id, array $current): void
    {
        if (!$scope->isOrganization() || RecordKind::tryFrom(self::string($current['kind']))?->keepsRevisions() !== true) {
            return;
        }
        $record = ['owner' => $scope->owner->toRfc4122(), 'id' => $id];
        $this->connection->executeStatement(
            'INSERT INTO record_revision (id, organization_id, record_id, version, payload, payload_bytes, updated_at, updated_by_id, updated_device_id)
             SELECT :revision, organization_id, id, version, payload, payload_bytes, updated_at, updated_by_id, updated_device_id
             FROM record WHERE organization_id = :owner AND id = :id',
            $record + ['revision' => Uuid::v7()->toRfc4122()],
        );
        $this->connection->executeStatement(
            'DELETE FROM record_revision WHERE organization_id = :owner AND record_id = :id AND version IN (
                SELECT version FROM (
                    SELECT version, ROW_NUMBER() OVER (ORDER BY version DESC) AS position, SUM(payload_bytes) OVER (ORDER BY version DESC) AS bytes
                    FROM record_revision WHERE organization_id = :owner AND record_id = :id
                ) newest_first WHERE position > :revisions OR (position > 1 AND bytes > :bytes)
             )',
            $record + ['revisions' => $this->maxRevisions, 'bytes' => $this->maxRevisionBytes],
        );
    }

    /**
     * How many live records the scope holds and the bytes of their payloads.
     *
     * @return array{records: int, bytes: int}
     */
    private function usage(SyncScope $scope): array
    {
        $usage = $this->connection->fetchAssociative(
            \sprintf('SELECT COUNT(*) AS records, COALESCE(SUM(payload_bytes), 0) AS bytes FROM %s WHERE %s = :owner AND deleted_at IS NULL', $scope->table, $scope->ownerColumn),
            ['owner' => $scope->owner->toRfc4122()],
        );
        if (false === $usage) {
            throw new \UnexpectedValueException('Expected the usage of a scope from the database.');
        }

        return ['records' => RecordRows::int($usage['records']), 'bytes' => RecordRows::int($usage['bytes'])];
    }

    /** @return array{id: string, status: 'accepted', version: int} */
    private static function accepted(Change $change, int $version): array
    {
        return ['id' => $change->id, 'status' => 'accepted', 'version' => $version];
    }

    /**
     * @param array<string, mixed>|null $current
     *
     * @return array{id: string, status: 'conflict', current: array<string, mixed>|null}
     */
    private static function conflict(Change $change, ?array $current, SyncScope $scope): array
    {
        return ['id' => $change->id, 'status' => 'conflict', 'current' => null === $current ? null : RecordRows::toArray($current, $scope)];
    }

    /**
     * Drops the writer's user id where the scope's table has no column for it.
     *
     * @param array<string, mixed> $parameters
     *
     * @return array<string, mixed>
     */
    private static function only(array $parameters, SyncScope $scope): array
    {
        if (!$scope->recordsAuthor) {
            unset($parameters['user']);
        }

        return $parameters;
    }

    private static function string(mixed $value): string
    {
        if (!\is_string($value)) {
            throw new \UnexpectedValueException('Expected a string from the database.');
        }

        return $value;
    }
}
