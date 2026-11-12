<?php

declare(strict_types=1);

namespace App\Organization;

use App\Entity\Organization;
use App\Entity\OrganizationMember;
use App\Entity\User;
use App\Enum\OrganizationRole;
use Doctrine\ORM\EntityManagerInterface;
use Psr\Clock\ClockInterface;

/**
 * Makes the personal organization a new user owns, with the user as its one member (one seat). Billing will hang a
 * subscription off it; nothing of that is here yet.
 */
final readonly class PersonalOrganizations
{
    public function __construct(
        private EntityManagerInterface $entityManager,
        private ClockInterface $clock,
    ) {
    }

    public function create(User $user): Organization
    {
        $now = $this->clock->now();
        $organization = Organization::personal($user, $now);
        $this->entityManager->persist($organization);
        $this->entityManager->persist(new OrganizationMember($organization, $user, OrganizationRole::Owner, $now));

        return $organization;
    }
}
