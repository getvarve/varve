<?php

declare(strict_types=1);

namespace App\Repository;

use App\Entity\AccountLink;
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
}
