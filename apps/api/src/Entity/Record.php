<?php

declare(strict_types=1);

namespace App\Entity;

use App\Enum\RecordKind;
use Doctrine\DBAL\Types\Types;
use Doctrine\ORM\Mapping as ORM;
use Symfony\Bridge\Doctrine\Types\UuidType;
use Symfony\Component\Uid\Uuid;

/**
 * A synced record of an organization (design 8.6 and 8.7): a connection profile, a folder or a saved query, as JSON the
 * client wrote. The client picks the id, so it is only unique within the organization; one organization can never
 * reach another's record by naming its id.
 *
 * `version` counts the accepted writes of this record (the client's base version), `change_seq` places the last one
 * in the organization's change feed ({@see Organization::$recordSeq}). A delete keeps the row as a tombstone without a
 * payload until App\Sync\PurgeSyncTombstonesHandler removes it after 90 days.
 *
 * Written and read by App\Sync in SQL, never through the ORM; mapped here for the schema.
 */
#[ORM\Entity(readOnly: true)]
#[ORM\UniqueConstraint(columns: ['organization_id', 'change_seq'])]
#[ORM\Index(columns: ['deleted_at'])]
class Record
{
    /** The longest name a folder, connection or saved query may have, as in the desktop app. */
    public const int MAX_NAME = 200;

    #[ORM\Id]
    #[ORM\ManyToOne]
    #[ORM\JoinColumn(nullable: false, onDelete: 'CASCADE')]
    private Organization $organization;

    #[ORM\Id]
    #[ORM\Column(type: UuidType::NAME)]
    private Uuid $id;

    #[ORM\Column(length: 16, enumType: RecordKind::class)]
    private RecordKind $kind;

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

    /** Who wrote the last change; null once that user is gone. */
    #[ORM\ManyToOne]
    #[ORM\JoinColumn(onDelete: 'SET NULL')]
    private ?User $updatedBy = null;

    /** The device that wrote the last change; null once it is signed out. */
    #[ORM\ManyToOne]
    #[ORM\JoinColumn(onDelete: 'SET NULL')]
    private ?Device $updatedDevice = null;

    /** Set when the record was deleted: it is a tombstone since. */
    #[ORM\Column(nullable: true)]
    private ?\DateTimeImmutable $deletedAt = null;
}
