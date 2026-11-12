<?php

declare(strict_types=1);

namespace App\Entity;

use Doctrine\DBAL\Types\Types;
use Doctrine\ORM\Mapping as ORM;
use Symfony\Bridge\Doctrine\Types\UuidType;
use Symfony\Component\Uid\Uuid;

/**
 * An earlier version of a saved query (design 8.11): every accepted change to one keeps the payload it replaced, with
 * who wrote it and when. The newest per record are kept, up to a number of revisions and a number of bytes that the
 * server sets (VARVE_SYNC_MAX_REVISIONS, 50 by design, and VARVE_SYNC_MAX_REVISION_BYTES); they go with the record.
 *
 * Written and read by App\Sync in SQL, never through the ORM; mapped here for the schema.
 */
#[ORM\Entity(readOnly: true)]
#[ORM\UniqueConstraint(columns: ['organization_id', 'record_id', 'version'])]
class RecordRevision
{
    #[ORM\Id]
    #[ORM\Column(type: UuidType::NAME)]
    private Uuid $id;

    #[ORM\ManyToOne]
    #[ORM\JoinColumn(name: 'organization_id', referencedColumnName: 'organization_id', nullable: false, onDelete: 'CASCADE')]
    #[ORM\JoinColumn(name: 'record_id', referencedColumnName: 'id', nullable: false, onDelete: 'CASCADE')]
    private Record $record;

    /** The version of the record this payload was. */
    #[ORM\Column]
    private int $version;

    /** @var array<mixed> */
    #[ORM\Column(type: Types::JSON)]
    private array $payload;

    /** The size of the payload as it is stored, in bytes. */
    #[ORM\Column(options: ['default' => 0])]
    private int $payloadBytes = 0;

    /** When that version was written. */
    #[ORM\Column]
    private \DateTimeImmutable $updatedAt;

    #[ORM\ManyToOne]
    #[ORM\JoinColumn(onDelete: 'SET NULL')]
    private ?User $updatedBy = null;

    #[ORM\ManyToOne]
    #[ORM\JoinColumn(onDelete: 'SET NULL')]
    private ?Device $updatedDevice = null;
}
