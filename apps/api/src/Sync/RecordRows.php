<?php

declare(strict_types=1);

namespace App\Sync;

/**
 * Turns record rows into what clients get, in a pull and as the server's copy in a conflict:
 *
 *     {id, kind, version, deleted, payload, createdAt, createdBy, updatedAt, updatedBy, updatedByDevice}
 *
 * `createdBy` and `updatedBy` are there for an organization's records only: a user's own records are all theirs.
 *
 * A tombstone has `deleted: true` and `payload: null`.
 */
final class RecordRows
{
    /** The columns {@see self::toArray()} needs, for a SELECT on the scope's table aliased `r`. */
    public static function columns(SyncScope $scope): string
    {
        return 'r.id, r.kind, r.version, r.payload::text AS payload, r.payload_bytes, r.created_at, r.updated_at, r.deleted_at, r.updated_device_id, r.change_seq'
            .($scope->recordsAuthor ? ', r.created_by_id, r.updated_by_id' : '');
    }

    /**
     * @param array<string, mixed> $row
     *
     * @return array<string, mixed>
     */
    public static function toArray(array $row, SyncScope $scope): array
    {
        $record = [
            'id' => self::string($row['id']),
            'kind' => self::string($row['kind']),
            'version' => self::int($row['version']),
            'deleted' => null !== $row['deleted_at'],
            'payload' => null === $row['payload'] ? null : json_decode(self::string($row['payload']), false, 512, \JSON_THROW_ON_ERROR),
            'createdAt' => self::date($row['created_at']),
        ];
        if ($scope->recordsAuthor) {
            $record['createdBy'] = null === $row['created_by_id'] ? null : self::string($row['created_by_id']);
        }
        $record['updatedAt'] = self::date($row['updated_at']);
        if ($scope->recordsAuthor) {
            $record['updatedBy'] = null === $row['updated_by_id'] ? null : self::string($row['updated_by_id']);
        }
        $record['updatedByDevice'] = null === $row['updated_device_id'] ? null : self::string($row['updated_device_id']);

        return $record;
    }

    public static function date(mixed $value): string
    {
        return new \DateTimeImmutable(self::string($value))->format(\DATE_ATOM);
    }

    public static function int(mixed $value): int
    {
        if (!is_numeric($value)) {
            throw new \UnexpectedValueException('Expected a number from the database.');
        }

        return (int) $value;
    }

    private static function string(mixed $value): string
    {
        if (!\is_string($value)) {
            throw new \UnexpectedValueException('Expected a string from the database.');
        }

        return $value;
    }
}
