<?php

declare(strict_types=1);

namespace App\Sync;

/**
 * A change the server will not store, whatever the record holds: the answer names the reason, the field (a path in
 * the change, such as `payload.ssh.password`) when one field is the cause, and a message for people.
 */
final readonly class Rejection
{
    /** The change or its payload breaks a rule of the envelope or of its kind. */
    public const string INVALID = 'invalid';
    /** The upsert names a kind this scope does not have. */
    public const string UNKNOWN_KIND = 'unknown_kind';
    /** The payload holds a secret (a password, passphrase, key or token field, or a PEM block). */
    public const string SECRET = 'secret';
    /** The payload is larger than {@see ChangeParser::MAX_PAYLOAD_BYTES}. */
    public const string TOO_LARGE = 'too_large';
    /** A delete of a record the server does not have. */
    public const string NOT_FOUND = 'not_found';
    /** The upsert would take the scope over its number of live records. */
    public const string RECORD_LIMIT = 'record_limit';

    public function __construct(
        /** The change's id when it had a valid one. */
        public ?string $id,
        public string $reason,
        public string $message,
        public ?string $field = null,
    ) {
    }

    /** @return array{id: ?string, status: 'rejected', reason: string, field: ?string, message: string} */
    public function toArray(): array
    {
        return ['id' => $this->id, 'status' => 'rejected', 'reason' => $this->reason, 'field' => $this->field, 'message' => $this->message];
    }
}
