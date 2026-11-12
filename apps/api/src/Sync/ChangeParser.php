<?php

declare(strict_types=1);

namespace App\Sync;

use App\Enum\RecordKind;
use App\Enum\UserRecordKind;
use App\State\Violation;
use Symfony\Component\HttpKernel\Exception\BadRequestHttpException;
use Symfony\Component\HttpKernel\Exception\HttpException;

/**
 * Reads the body of a push: `{"changes": [...]}` with at most {@see self::MAX_CHANGES} changes. A body that is not
 * that fails as a whole (400, 413 or 422); a change that breaks a rule of its own is rejected on its own, and the
 * others go on.
 *
 * Payloads are decoded with JSON objects as objects, so an empty object stays `{}` and an object with keys "0", "1"
 * stays an object: what a client pulls is what it pushed, give or take whitespace and the order of keys.
 */
final class ChangeParser
{
    public const int MAX_CHANGES = 100;
    /** 256 KiB: the stored JSON of one payload (design 8.8), large SQL scripts included. */
    public const int MAX_PAYLOAD_BYTES = 262144;
    /** A whole push; split a larger one. */
    public const int MAX_BODY_BYTES = 8388608;
    /** How many levels of objects and lists may nest below a payload (the payload itself is level 0). */
    public const int MAX_DEPTH = 32;
    /** 2^63: numbers must lie in the range of a signed 64-bit integer, or they would not come back as sent. */
    private const float MAX_NUMBER = 9.2233720368547758E18;

    private const array UPSERT_FIELDS = ['op', 'id', 'kind', 'baseVersion', 'payload'];
    private const array DELETE_FIELDS = ['op', 'id', 'baseVersion'];
    private const int MAX_VERSION = 2147483647;

    /**
     * @return list<Change|Rejection> one per change, in the order of the push
     */
    public static function parse(string $body, SyncScope $scope): array
    {
        if (\strlen($body) > self::MAX_BODY_BYTES) {
            throw self::bodyTooLarge();
        }
        try {
            $decoded = json_decode($body, false, 512, \JSON_THROW_ON_ERROR);
        } catch (\JsonException $exception) {
            // Valid JSON that PHP cannot hold as an object. It cannot be pinned on one change, so the push fails.
            if (\JSON_ERROR_INVALID_PROPERTY_NAME === $exception->getCode()) {
                throw Violation::on('changes', 'The name of a field cannot start with a NUL character (\u0000).');
            }
            throw new BadRequestHttpException('The body is not valid JSON.');
        }
        if (!$decoded instanceof \stdClass || !\is_array($decoded->changes ?? null)) {
            throw Violation::on('changes', 'The body must be an object with a list of changes.');
        }
        if (\count($decoded->changes) > self::MAX_CHANGES) {
            throw Violation::on('changes', \sprintf('A push can hold at most %d changes.', self::MAX_CHANGES));
        }

        return array_map(static fn (mixed $change): Change|Rejection => self::change($change, $scope), array_values($decoded->changes));
    }

    /** The 413 for a push whose body is larger than {@see self::MAX_BODY_BYTES}. */
    public static function bodyTooLarge(): HttpException
    {
        return new HttpException(413, \sprintf('A push can be at most %d bytes; send the changes in more than one push.', self::MAX_BODY_BYTES));
    }

    private static function change(mixed $change, SyncScope $scope): Change|Rejection
    {
        if (!$change instanceof \stdClass) {
            return new Rejection(null, Rejection::INVALID, 'A change must be an object.');
        }
        $fields = get_object_vars($change);
        $id = $fields['id'] ?? null;
        if (!\is_string($id) || 1 !== preg_match(PayloadRules::UUID, $id)) {
            return new Rejection(null, Rejection::INVALID, 'id must be the record\'s id, a lower-case UUID.', 'id');
        }

        $op = $fields['op'] ?? null;
        if ('upsert' !== $op && 'delete' !== $op) {
            return new Rejection($id, Rejection::INVALID, 'op must be "upsert" or "delete".', 'op');
        }
        foreach (array_keys($fields) as $field) {
            if (!\in_array($field, 'upsert' === $op ? self::UPSERT_FIELDS : self::DELETE_FIELDS, true)) {
                $name = Rejection::path((string) $field);

                return new Rejection($id, Rejection::INVALID, \sprintf('A change of op "%s" has no field %s.', $op, $name), $name);
            }
        }
        $baseVersion = $fields['baseVersion'] ?? null;
        if (!\is_int($baseVersion) || $baseVersion < 0 || $baseVersion > self::MAX_VERSION) {
            return new Rejection($id, Rejection::INVALID, 'baseVersion must be the version of the record the change is based on, 0 for a new record.', 'baseVersion');
        }
        if ('delete' === $op) {
            return Change::delete($id, $baseVersion);
        }

        $kindName = $fields['kind'] ?? null;
        $kind = \is_string($kindName) ? $scope->kind($kindName) : null;
        if (null === $kind) {
            return new Rejection($id, Rejection::UNKNOWN_KIND, \sprintf('kind must be one of: %s.', implode(', ', self::kindNames($scope))), 'kind');
        }
        $payload = $fields['payload'] ?? null;
        if (!$payload instanceof \stdClass) {
            return new Rejection($id, Rejection::INVALID, 'payload must be an object.', 'payload');
        }

        // Before the payload is encoded: a number too large for a double (1e999) cannot be, and is refused here.
        $shape = self::shape($payload, 'payload', 0);
        if (null !== $shape) {
            return new Rejection($id, Rejection::INVALID, $shape['message'], $shape['field']);
        }
        $stored = json_encode($payload, \JSON_THROW_ON_ERROR | \JSON_UNESCAPED_SLASHES | \JSON_UNESCAPED_UNICODE | \JSON_PRESERVE_ZERO_FRACTION);
        if (\strlen($stored) > self::MAX_PAYLOAD_BYTES) {
            return new Rejection(
                $id, Rejection::TOO_LARGE,
                \sprintf('payload is %d bytes as JSON; at most %d are allowed.', \strlen($stored), self::MAX_PAYLOAD_BYTES),
                'payload', self::MAX_PAYLOAD_BYTES, \strlen($stored),
            );
        }
        $secret = SecretGuard::find($payload);
        if (null !== $secret) {
            return new Rejection($id, Rejection::SECRET, $secret['message'], $secret['field']);
        }
        $problem = PayloadRules::check($kind, $id, $payload);
        if (null !== $problem) {
            return new Rejection($id, Rejection::INVALID, $problem['message'], $problem['field']);
        }

        return Change::upsert($id, $baseVersion, $kind, $stored);
    }

    /**
     * Nesting and numbers a payload may not have. `$depth` is 0 for the payload and one more for every object or list
     * inside another.
     *
     * @return array{field: string, message: string}|null
     */
    private static function shape(mixed $value, string $path, int $depth): ?array
    {
        if (\is_float($value)) {
            // Integers beyond 64 bits arrive as floats; a float that large would come back in another notation.
            return $value >= self::MAX_NUMBER || $value < -self::MAX_NUMBER
                ? ['field' => Rejection::path($path), 'message' => \sprintf('%s is a number beyond a signed 64-bit integer.', Rejection::path($path))]
                : null;
        }
        if (!\is_array($value) && !$value instanceof \stdClass) {
            return null;
        }
        if ($depth > self::MAX_DEPTH) {
            return ['field' => Rejection::path($path), 'message' => \sprintf('%s nests more than %d levels below the payload.', Rejection::path($path), self::MAX_DEPTH)];
        }
        foreach (\is_array($value) ? $value : get_object_vars($value) as $key => $item) {
            $found = self::shape($item, \is_array($value) ? \sprintf('%s[%d]', $path, $key) : $path.'.'.$key, $depth + 1);
            if (null !== $found) {
                return $found;
            }
        }

        return null;
    }

    /** @return list<string> */
    private static function kindNames(SyncScope $scope): array
    {
        return $scope->isOrganization()
            ? array_map(static fn (RecordKind $kind): string => $kind->value, RecordKind::cases())
            : array_map(static fn (UserRecordKind $kind): string => $kind->value, UserRecordKind::cases());
    }
}
