<?php

declare(strict_types=1);

namespace App\Repository;

use App\Entity\AccountLink;
use App\Entity\User;
use App\Enum\AccountLinkPurpose;
use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;
use Doctrine\Persistence\ManagerRegistry;

/**
 * @extends ServiceEntityRepository<AccountLink>
 */
class AccountLinkRepository extends ServiceEntityRepository
{
    public function __construct(ManagerRegistry $registry)
    {
        parent::__construct($registry, AccountLink::class);
    }

    public function findOneByTokenHash(string $tokenHash, AccountLinkPurpose $purpose): ?AccountLink
    {
        return $this->findOneBy(['tokenHash' => $tokenHash, 'purpose' => $purpose]);
    }

    /** The user's latest link of this purpose that the table still holds, working or expired. */
    public function findNewest(User $user, AccountLinkPurpose $purpose): ?AccountLink
    {
        return $this->findOneBy(['user' => $user, 'purpose' => $purpose], ['createdAt' => 'DESC', 'id' => 'DESC']);
    }
}
