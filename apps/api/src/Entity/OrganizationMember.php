<?php

declare(strict_types=1);

namespace App\Entity;

use App\Enum\OrganizationRole;
use App\Repository\OrganizationMemberRepository;
use Doctrine\ORM\Mapping as ORM;
use Symfony\Bridge\Doctrine\Types\UuidType;
use Symfony\Component\Uid\Uuid;

#[ORM\Entity(repositoryClass: OrganizationMemberRepository::class)]
#[ORM\UniqueConstraint(columns: ['organization_id', 'user_id'])]
#[ORM\Index(columns: ['user_id'])]
class OrganizationMember
{
    #[ORM\Id]
    #[ORM\Column(type: UuidType::NAME)]
    private Uuid $id;

    #[ORM\ManyToOne]
    #[ORM\JoinColumn(nullable: false, onDelete: 'CASCADE')]
    private Organization $organization;

    #[ORM\ManyToOne]
    #[ORM\JoinColumn(nullable: false, onDelete: 'CASCADE')]
    private User $user;

    #[ORM\Column(length: 16, enumType: OrganizationRole::class)]
    private OrganizationRole $role;

    #[ORM\Column]
    private \DateTimeImmutable $joinedAt;

    public function __construct(Organization $organization, User $user, OrganizationRole $role, \DateTimeImmutable $joinedAt)
    {
        $this->id = Uuid::v7();
        $this->organization = $organization;
        $this->user = $user;
        $this->role = $role;
        $this->joinedAt = $joinedAt;
    }

    public function getId(): Uuid
    {
        return $this->id;
    }

    public function getOrganization(): Organization
    {
        return $this->organization;
    }

    public function getUser(): User
    {
        return $this->user;
    }

    public function getRole(): OrganizationRole
    {
        return $this->role;
    }

    public function getJoinedAt(): \DateTimeImmutable
    {
        return $this->joinedAt;
    }
}
