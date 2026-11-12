<?php

declare(strict_types=1);

namespace App\Sync;

use App\Enum\RecordKind;
use App\Enum\UserRecordKind;

/**
 * One change of a push that passed every check on its own: an upsert with its kind and payload (as the JSON that is
 * stored), or a delete. Whether it is accepted depends on the record as it is when the push gets to it.
 */
final readonly class Change
{
    private function __construct(
        public string $id,
        public int $baseVersion,
        public RecordKind|UserRecordKind|null $kind,
        public ?string $payload,
    ) {
    }

    public static function upsert(string $id, int $baseVersion, RecordKind|UserRecordKind $kind, string $payload): self
    {
        return new self($id, $baseVersion, $kind, $payload);
    }

    public static function delete(string $id, int $baseVersion): self
    {
        return new self($id, $baseVersion, null, null);
    }

    public function isDelete(): bool
    {
        return null === $this->kind;
    }
}
