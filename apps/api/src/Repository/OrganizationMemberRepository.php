<?php

declare(strict_types=1);

namespace App\Repository;

use App\Entity\Organization;
use App\Entity\OrganizationMember;
use App\Entity\User;
use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;
use Doctrine\Persistence\ManagerRegistry;
use Symfony\Component\Uid\Uuid;

/**
 * @extends ServiceEntityRepository<OrganizationMember>
 */
class OrganizationMemberRepository extends ServiceEntityRepository
{
    public function __construct(ManagerRegistry $registry)
    {
        parent::__construct($registry, OrganizationMember::class);
    }

    /**
     * The user's memberships: their own personal organization first, then the others by when the user joined.
     *
     * @return list<OrganizationMember>
     */
    public function findOfUser(User $user): array
    {
        /** @var list<OrganizationMember> */
        return $this->createQueryBuilder('m')
            ->addSelect('o')
            ->join('m.organization', 'o')
            ->where('m.user = :user')
            ->setParameter('user', $user->getId(), 'uuid')
            ->addSelect('CASE WHEN o.personalOwner = :user THEN 0 ELSE 1 END AS HIDDEN ownFirst')
            ->orderBy('ownFirst')
            ->addOrderBy('m.joinedAt')
            ->addOrderBy('m.id')
            ->getQuery()
            ->getResult();
    }

    /** The organization every user owns from the start. */
    public function personalOrganizationOf(User $user): Organization
    {
        $organization = $this->getEntityManager()->getRepository(Organization::class)->findOneBy(['personalOwner' => $user]);
        if (!$organization instanceof Organization) {
            throw new \LogicException(\sprintf('User %s has no personal organization.', $user->getId()->toRfc4122()));
        }

        return $organization;
    }

    /**
     * Members per organization, which are its seats.
     *
     * @param list<Uuid> $organizations
     *
     * @return array<string, int> keyed by organization id (RFC 4122)
     */
    public function countPerOrganization(array $organizations): array
    {
        if ([] === $organizations) {
            return [];
        }

        /** @var list<array{organization_id: string, seats: int}> $rows */
        $rows = $this->getEntityManager()->getConnection()->fetchAllAssociative(
            'SELECT organization_id, COUNT(*) AS seats FROM organization_member
             WHERE organization_id = ANY(CAST(:ids AS UUID[])) GROUP BY organization_id',
            ['ids' => '{'.implode(',', array_map(static fn (Uuid $id): string => $id->toRfc4122(), $organizations)).'}'],
        );

        return array_column($rows, 'seats', 'organization_id');
    }
}
