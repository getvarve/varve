<?php

declare(strict_types=1);

namespace App\Entity;

use Doctrine\ORM\Mapping as ORM;
use Symfony\Bridge\Doctrine\Types\UuidType;
use Symfony\Component\Uid\Uuid;

/**
 * The people who share a subscription: a team, or one user on their own. Every user has one personal organization,
 * made with the user and removed with them; its members are its seats. A team has a name and no personal owner, a
 * personal organization the other way round (a check constraint keeps it so). Teams come later; for now every
 * organization is personal.
 */
#[ORM\Entity]
class Organization
{
    public const int MAX_NAME = 120;

    #[ORM\Id]
    #[ORM\Column(type: UuidType::NAME)]
    private Uuid $id;

    /** null for a personal organization, which goes by its owner's name. */
    #[ORM\Column(length: self::MAX_NAME, nullable: true)]
    private ?string $name;

    /** The user whose personal organization this is; null for a team. */
    #[ORM\OneToOne]
    #[ORM\JoinColumn(unique: true, onDelete: 'CASCADE')]
    private ?User $personalOwner;

    #[ORM\Column]
    private \DateTimeImmutable $createdAt;

    private function __construct(?string $name, ?User $personalOwner, \DateTimeImmutable $createdAt)
    {
        $this->id = Uuid::v7();
        $this->name = $name;
        $this->personalOwner = $personalOwner;
        $this->createdAt = $createdAt;
    }

    public static function personal(User $owner, \DateTimeImmutable $createdAt): self
    {
        return new self(null, $owner, $createdAt);
    }

    public function getId(): Uuid
    {
        return $this->id;
    }

    public function getName(): string
    {
        return $this->personalOwner?->getDisplayName() ?? (string) $this->name;
    }

    public function isPersonalOf(User $user): bool
    {
        return null !== $this->personalOwner && $this->personalOwner->getId()->equals($user->getId());
    }

    public function getCreatedAt(): \DateTimeImmutable
    {
        return $this->createdAt;
    }
}
