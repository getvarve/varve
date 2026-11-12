<?php

declare(strict_types=1);

namespace App\Account;

use App\Entity\AccountLink;
use App\Entity\User;
use App\Enum\AccountLinkPurpose;
use App\Repository\AccountLinkRepository;
use Doctrine\ORM\EntityManagerInterface;
use Psr\Clock\ClockInterface;

/**
 * Makes and checks the links in account mail ({@see AccountLink}). A link's secret is 32 random bytes, hex encoded;
 * only its hash is stored.
 */
final readonly class AccountLinks
{
    public function __construct(
        private EntityManagerInterface $entityManager,
        private AccountLinkRepository $links,
        private ClockInterface $clock,
    ) {
    }

    /**
     * A new link for the user, which ends every earlier one of this purpose. Returns its secret.
     *
     * @param string|null $pendingDisplayName the name of the sign-up a confirmation link is for
     * @param string|null $pendingPassword    the password hash of that sign-up
     */
    public function issue(User $user, AccountLinkPurpose $purpose, ?string $pendingDisplayName = null, #[\SensitiveParameter] ?string $pendingPassword = null): string
    {
        $this->end($user, $purpose);

        $secret = bin2hex(random_bytes(32));
        $this->entityManager->persist(new AccountLink($user, $purpose, self::hash($secret), $this->clock->now(), $pendingDisplayName, $pendingPassword));
        $this->entityManager->flush();

        return $secret;
    }

    /** The link a secret names, while it still works; null for an unknown, used or expired one. */
    public function find(#[\SensitiveParameter] string $secret, AccountLinkPurpose $purpose): ?AccountLink
    {
        $link = $this->known($secret, $purpose);

        return null === $link || $link->isExpiredAt($this->clock->now()) ? null : $link;
    }

    /**
     * The link a secret names, working or expired: it sets or confirms nothing by itself, but whoever holds it was
     * sent it. Null for an unknown one, a used one, and one a newer link replaced.
     */
    public function known(#[\SensitiveParameter] string $secret, AccountLinkPurpose $purpose): ?AccountLink
    {
        if (1 !== preg_match('/^[0-9a-f]{64}$/', $secret)) {
            return null;
        }

        return $this->links->findOneByTokenHash(self::hash($secret), $purpose);
    }

    /** The user's latest link of this purpose, working or expired. */
    public function newest(User $user, AccountLinkPurpose $purpose): ?AccountLink
    {
        return $this->links->findNewest($user, $purpose);
    }

    /** Ends every link of this purpose the user has, at once. */
    public function end(User $user, AccountLinkPurpose $purpose): void
    {
        $this->entityManager->createQueryBuilder()
            ->delete(AccountLink::class, 'l')
            ->where('l.user = :user')
            ->andWhere('l.purpose = :purpose')
            ->setParameter('user', $user->getId(), 'uuid')
            ->setParameter('purpose', $purpose->value)
            ->getQuery()
            ->execute();
    }

    private static function hash(#[\SensitiveParameter] string $secret): string
    {
        return hash('sha256', $secret);
    }
}
