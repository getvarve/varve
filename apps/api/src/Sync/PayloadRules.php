<?php

declare(strict_types=1);

namespace App\Sync;

use App\Entity\Record;
use App\Enum\RecordKind;
use App\Enum\UserRecordKind;

/**
 * The basics every kind of record must have, so the server cannot fill up with garbage. The server never connects to
 * a database and does not read SQL: it checks names, references and types of the fields the desktop app writes
 * (crates/records, design 3.1 and 3.4), and keeps every other field as it came, since a newer app may write fields this
 * server does not know.
 *
 * References to other records (`parentId`, `folderId`, `connectionId`, `recordId`) are checked for their shape only: a
 * client may push a record before the folder it sits in, and resolves a reference to a record it does not have as
 * "none".
 */
final class PayloadRules
{
    public const string UUID = '/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/';

    private const int MAX_COLOR = 32;
    private const int MAX_TEXT = 255;
    private const int MAX_ENGINE = 32;
    private const int MAX_TAG = 100;
    private const int MAX_TAGS = 100;

    /**
     * What is wrong with the payload of a record of this kind with this id; null when nothing is.
     *
     * @return array{field: string, message: string}|null
     */
    public static function check(RecordKind|UserRecordKind $kind, string $id, \stdClass $payload): ?array
    {
        $fields = get_object_vars($payload);
        if (\array_key_exists('id', $fields) && $fields['id'] !== $id) {
            return self::problem('id', 'must be the id of the change when it is present.');
        }

        return match ($kind) {
            RecordKind::Folder => self::named($fields)
                ?? self::reference($fields, 'parentId')
                ?? (($fields['parentId'] ?? null) === $id ? self::problem('parentId', 'cannot be the folder itself.') : null)
                ?? self::placed($fields),
            RecordKind::Connection => self::named($fields)
                ?? self::reference($fields, 'folderId')
                ?? self::placed($fields)
                ?? self::engine($fields)
                ?? self::text($fields, 'host')
                ?? self::port($fields)
                ?? self::optionalText($fields, 'database')
                ?? self::optionalText($fields, 'username')
                ?? self::object($fields, 'tls')
                ?? self::object($fields, 'ssh')
                ?? self::object($fields, 'options'),
            RecordKind::Query => self::named($fields)
                ?? self::reference($fields, 'folderId')
                ?? self::reference($fields, 'connectionId')
                ?? self::placed($fields)
                ?? self::optionalText($fields, 'database')
                ?? (\is_string($fields['sql'] ?? null) ? null : self::problem('sql', 'is required and must be a string.'))
                ?? self::tags($fields),
            UserRecordKind::Favourite => self::requiredReference($fields, 'recordId'),
            UserRecordKind::Override => self::requiredReference($fields, 'connectionId'),
            UserRecordKind::Preference => null,
        };
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function named(array $fields): ?array
    {
        $name = $fields['name'] ?? null;
        if (!\is_string($name) || '' === trim($name)) {
            return self::problem('name', 'is required and cannot be blank.');
        }
        if (mb_strlen($name) > Record::MAX_NAME) {
            return self::problem('name', \sprintf('can be at most %d characters.', Record::MAX_NAME));
        }

        return null;
    }

    /**
     * The color and manual order every item of the folder tree has.
     *
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function placed(array $fields): ?array
    {
        $color = $fields['color'] ?? null;
        if (null !== $color && (!\is_string($color) || mb_strlen($color) > self::MAX_COLOR)) {
            return self::problem('color', \sprintf('must be null or a string of at most %d characters.', self::MAX_COLOR));
        }
        $order = $fields['sortOrder'] ?? null;
        if (null !== $order && !\is_int($order)) {
            return self::problem('sortOrder', 'must be null or an integer.');
        }

        return null;
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function reference(array $fields, string $field): ?array
    {
        $value = $fields[$field] ?? null;

        return null === $value || (\is_string($value) && 1 === preg_match(self::UUID, $value))
            ? null
            : self::problem($field, 'must be null or the id of a record (a lower-case UUID).');
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function requiredReference(array $fields, string $field): ?array
    {
        $value = $fields[$field] ?? null;

        return \is_string($value) && 1 === preg_match(self::UUID, $value)
            ? null
            : self::problem($field, 'is required and must be the id of a record (a lower-case UUID).');
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function engine(array $fields): ?array
    {
        $engine = $fields['engine'] ?? null;

        return \is_string($engine) && 1 === preg_match('/^[a-z0-9_]{1,'.self::MAX_ENGINE.'}$/', $engine)
            ? null
            : self::problem('engine', 'is required: the name of a database engine, such as "postgres".');
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function text(array $fields, string $field): ?array
    {
        if (!\array_key_exists($field, $fields)) {
            return null;
        }
        $value = $fields[$field];

        return \is_string($value) && \strlen($value) <= self::MAX_TEXT
            ? null
            : self::problem($field, \sprintf('must be a string of at most %d bytes.', self::MAX_TEXT));
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function optionalText(array $fields, string $field): ?array
    {
        $value = $fields[$field] ?? null;

        return null === $value || (\is_string($value) && \strlen($value) <= self::MAX_TEXT)
            ? null
            : self::problem($field, \sprintf('must be null or a string of at most %d bytes.', self::MAX_TEXT));
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function port(array $fields): ?array
    {
        $port = $fields['port'] ?? null;

        return null === $port || (\is_int($port) && $port >= 0 && $port <= 65535)
            ? null
            : self::problem('port', 'must be null or a whole number from 0 to 65535.');
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function object(array $fields, string $field): ?array
    {
        $value = $fields[$field] ?? null;

        return null === $value || $value instanceof \stdClass ? null : self::problem($field, 'must be null or an object.');
    }

    /**
     * @param array<array-key, mixed> $fields
     *
     * @return array{field: string, message: string}|null
     */
    private static function tags(array $fields): ?array
    {
        $tags = $fields['tags'] ?? null;
        if (null === $tags) {
            return null;
        }
        if (!\is_array($tags) || \count($tags) > self::MAX_TAGS) {
            return self::problem('tags', \sprintf('must be null or a list of at most %d tags.', self::MAX_TAGS));
        }
        foreach ($tags as $index => $tag) {
            if (!\is_string($tag) || '' === trim($tag) || mb_strlen($tag) > self::MAX_TAG) {
                return self::problem(\sprintf('tags[%d]', $index), \sprintf('must be a non-blank string of at most %d characters.', self::MAX_TAG));
            }
        }

        return null;
    }

    /** @return array{field: string, message: string} */
    private static function problem(string $field, string $message): array
    {
        return ['field' => 'payload.'.$field, 'message' => 'payload.'.$field.' '.$message];
    }
}
