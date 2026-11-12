<?php

declare(strict_types=1);

namespace App\Sync;

use App\Enum\RecordKind;
use App\Enum\UserRecordKind;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;
use Symfony\Component\Uid\Uuid;

/**
 * One change feed: the records of an organization, or a user's own records (design 8.8: one cursor per organization
 * plus one for the per-user records). Both work the same way and differ only in their tables, their kinds and what
 * they keep: who wrote a change, and revisions of saved queries, belong to an organization's records only.
 */
final readonly class SyncScope
{
    private function __construct(
        /** The records' table. */
        public string $table,
        /** The column of {@see self::$table} naming the owner. */
        public string $ownerColumn,
        /** The owner's table, whose row holds record_seq and record_purge_seq. */
        public string $ownerTable,
        public Uuid $owner,
        /** Whether a record says which user wrote it (in an organization several can). */
        public bool $recordsAuthor,
    ) {
    }

    public static function organization(Uuid $organization): self
    {
        return new self('record', 'organization_id', 'organization', $organization, true);
    }

    public static function user(Uuid $user): self
    {
        return new self('user_record', 'user_id', 'app_user', $user, false);
    }

    public function isOrganization(): bool
    {
        return 'organization' === $this->ownerTable;
    }

    /** The kind with this name in this scope; null for one it does not have. */
    public function kind(string $name): RecordKind|UserRecordKind|null
    {
        return $this->isOrganization() ? RecordKind::tryFrom($name) : UserRecordKind::tryFrom($name);
    }

    /** The 404 for a scope whose owner is gone: the account or organization was deleted while a request was on its way. */
    public function notFound(): NotFoundHttpException
    {
        return new NotFoundHttpException($this->isOrganization() ? 'Organization not found.' : 'User not found.');
    }

    /** What a cursor of this scope is bound to, so it cannot be used for another. */
    public function cursorTag(): string
    {
        return ($this->isOrganization() ? 'o' : 'u').$this->owner->toRfc4122();
    }
}
