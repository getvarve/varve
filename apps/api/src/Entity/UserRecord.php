<?php

declare(strict_types=1);

namespace App\Entity;

use App\Enum\UserRecordKind;
use Doctrine\DBAL\Types\Types;
use Doctrine\ORM\Mapping as ORM;
use Symfony\Bridge\Doctrine\Types\UuidType;
use Symfony\Component\Uid\Uuid;

/**
 * A per-user record (design 8.1 and 8.6): a favourite, a preference or a personal override. It belongs to the account
 * and syncs between that account's devices only, with the same versions, change feed and tombstones as a
 * {@see Record}; its change numbers come from {@see User::$recordSeq}.
 *
 * Written and read by App\Sync in SQL, never through the ORM; mapped here for the schema.
 */
#[ORM\Entity(readOnly: true)]
#[ORM\UniqueConstraint(columns: ['user_id', 'change_seq'])]
#[ORM\Index(columns: ['deleted_at'])]
class UserRecord
{
    #[ORM\Id]
    #[ORM\ManyToOne]
    #[ORM\JoinColumn(nullable: false, onDelete: 'CASCADE')]
    private User $user;

    #[ORM\Id]
    #[ORM\Column(type: UuidType::NAME)]
    private Uuid $id;

    #[ORM\Column(length: 16, enumType: UserRecordKind::class)]
    private UserRecordKind $kind;

    /** @var array<mixed>|null null for a tombstone */
    #[ORM\Column(type: Types::JSON, nullable: true)]
    private ?array $payload = null;

    #[ORM\Column]
    private int $version;

    #[ORM\Column(type: Types::BIGINT)]
    private int $changeSeq;

    #[ORM\Column]
    private \DateTimeImmutable $createdAt;

    #[ORM\Column]
    private \DateTimeImmutable $updatedAt;

    /** The device that wrote the last change; null once it is signed out. */
    #[ORM\ManyToOne]
    #[ORM\JoinColumn(onDelete: 'SET NULL')]
    private ?Device $updatedDevice = null;

    /** Set when the record was deleted: it is a tombstone since. */
    #[ORM\Column(nullable: true)]
    private ?\DateTimeImmutable $deletedAt = null;
}
