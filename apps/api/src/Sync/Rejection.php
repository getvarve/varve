<?php

declare(strict_types=1);

namespace App\Sync;

/**
 * A change the server will not store, whatever the record holds: the answer names the reason, the field (a path in
 * the change, such as `payload.ssh.password`) when one field is the cause, and a message for people. Where a limit is
 * the reason, the answer carries the limit, and for a payload that is too large its size, as numbers a client can show.
 */
final readonly class Rejection
{
    /** The change or its payload breaks a rule of the envelope or of its kind. */
    public const string INVALID = 'invalid';
    /** The upsert names a kind this scope does not have. */
    public const string UNKNOWN_KIND = 'unknown_kind';
    /** The payload holds a secret ({@see SecretGuard}). */
    public const string SECRET = 'secret';
    /** The payload is larger than {@see ChangeParser::MAX_PAYLOAD_BYTES}. */
    public const string TOO_LARGE = 'too_large';
    /** A delete of a record the server does not have. */
    public const string NOT_FOUND = 'not_found';
    /** The upsert would take the scope over its number of live records. */
    public const string RECORD_LIMIT = 'record_limit';
    /** The upsert would take the payloads of the scope's live records over its number of bytes. */
    public const string STORAGE_LIMIT = 'storage_limit';

    /** A path is cut to this many bytes: a key of a payload can be as long as the payload itself. */
    public const int MAX_PATH_BYTES = 200;

    public function __construct(
        /** The change's id when it had a valid one. */
        public ?string $id,
        public string $reason,
        public string $message,
        public ?string $field = null,
        /** The limit the change went over, for the reasons that are about one. */
        public ?int $limit = null,
        /** The size of the payload in bytes, when it is too large. */
        public ?int $size = null,
    ) {
    }

    /**
     * A path as an answer carries it, in `field` and inside `message`: as it is, or its first
     * {@see self::MAX_PATH_BYTES} bytes and three dots.
     */
    public static function path(string $path): string
    {
        return \strlen($path) <= self::MAX_PATH_BYTES ? $path : mb_strcut($path, 0, self::MAX_PATH_BYTES, 'UTF-8').'...';
    }

    /** @return array{id: ?string, status: 'rejected', reason: string, field: ?string, message: string, limit?: int, size?: int} */
    public function toArray(): array
    {
        $result = ['id' => $this->id, 'status' => 'rejected', 'reason' => $this->reason, 'field' => $this->field, 'message' => $this->message];
        if (null !== $this->limit) {
            $result['limit'] = $this->limit;
        }
        if (null !== $this->size) {
            $result['size'] = $this->size;
        }

        return $result;
    }
}
