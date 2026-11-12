<?php

declare(strict_types=1);

namespace App\Account;

use App\Enum\AccountLinkPurpose;
use Doctrine\ORM\EntityManagerInterface;
use Psr\Clock\ClockInterface;

/**
 * Confirms the address of a new account with the link mailed to it. The link works once, for two days, and only while
 * it is the latest one sent.
 */
final readonly class EmailConfirmation
{
    public function __construct(
        private AccountLinks $links,
        private EntityManagerInterface $entityManager,
        private ClockInterface $clock,
    ) {
    }

    /** Whether the link would still confirm an address. */
    public function isValid(#[\SensitiveParameter] string $secret): bool
    {
        return null !== $this->links->find($secret, AccountLinkPurpose::ConfirmEmail);
    }

    /** Confirms the address the link was sent to; false when the link does not work (any more). */
    public function confirm(#[\SensitiveParameter] string $secret): bool
    {
        $link = $this->links->find($secret, AccountLinkPurpose::ConfirmEmail);
        if (null === $link) {
            return false;
        }
        $link->getUser()->confirmEmail($this->clock->now());
        $this->links->spend($link);
        $this->entityManager->flush();

        return true;
    }
}
