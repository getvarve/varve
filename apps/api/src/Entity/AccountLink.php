<?php

declare(strict_types=1);

namespace App\Entity;

use App\Enum\AccountLinkPurpose;
use App\Repository\AccountLinkRepository;
use Doctrine\ORM\Mapping as ORM;
use Symfony\Bridge\Doctrine\Types\UuidType;
use Symfony\Component\Uid\Uuid;

/**
 * A link sent in account mail: only the SHA-256 of its secret is kept, so the table gives nobody a working link. It
 * works once, until it expires, and only while it is the user's latest link for its purpose: asking for a new one
 * ends the old one.
 */
#[ORM\Entity(repositoryClass: AccountLinkRepository::class)]
#[ORM\UniqueConstraint(columns: ['token_hash'])]
#[ORM\Index(columns: ['user_id', 'purpose'])]
#[ORM\Index(columns: ['expires_at'])]
class AccountLink
{
    #[ORM\Id]
    #[ORM\Column(type: UuidType::NAME)]
    private Uuid $id;

    #[ORM\ManyToOne]
    #[ORM\JoinColumn(nullable: false, onDelete: 'CASCADE')]
    private User $user;

    #[ORM\Column(length: 16, enumType: AccountLinkPurpose::class)]
    private AccountLinkPurpose $purpose;

    /** SHA-256 of the secret in the link, hex. */
    #[ORM\Column(length: 64, options: ['fixed' => true])]
    private string $tokenHash;

    #[ORM\Column]
    private \DateTimeImmutable $createdAt;

    #[ORM\Column]
    private \DateTimeImmutable $expiresAt;

    public function __construct(User $user, AccountLinkPurpose $purpose, string $tokenHash, \DateTimeImmutable $createdAt)
    {
        $this->id = Uuid::v7();
        $this->user = $user;
        $this->purpose = $purpose;
        $this->tokenHash = $tokenHash;
        $this->createdAt = $createdAt;
        $this->expiresAt = $createdAt->add($purpose->lifetime());
    }

    public function getId(): Uuid
    {
        return $this->id;
    }

    public function getUser(): User
    {
        return $this->user;
    }

    public function getPurpose(): AccountLinkPurpose
    {
        return $this->purpose;
    }

    public function getCreatedAt(): \DateTimeImmutable
    {
        return $this->createdAt;
    }

    public function getExpiresAt(): \DateTimeImmutable
    {
        return $this->expiresAt;
    }

    public function isExpiredAt(\DateTimeImmutable $now): bool
    {
        return $now >= $this->expiresAt;
    }
}
