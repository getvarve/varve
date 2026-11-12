<?php

declare(strict_types=1);

namespace App\Account;

use Doctrine\DBAL\Connection;
use Psr\Clock\ClockInterface;
use Symfony\Component\Messenger\Attribute\AsMessageHandler;

/**
 * An account that was never confirmed cannot sign in and holds nothing but its personal organization. A week after it
 * was made, and once no confirmation link for it works any more, it goes (the database cascades to its organization
 * and its links), so an address someone signed up with by mistake, or on someone else's behalf, is free again.
 *
 * A link goes a week after it expired. Until then it works for nothing but one thing: whoever holds it was sent it,
 * and may ask for a new one without counting against what strangers asked for that address ({@see AccountMailBudget}).
 */
#[AsMessageHandler]
final readonly class PurgeAbandonedSignUpsHandler
{
    public const string KEEP_UNCONFIRMED = 'P7D';
    public const string KEEP_EXPIRED_LINKS = 'P7D';

    public function __construct(
        private Connection $connection,
        private ClockInterface $clock,
    ) {
    }

    /** @return int how many accounts and links were deleted together */
    public function __invoke(PurgeAbandonedSignUps $message): int
    {
        $now = $this->clock->now();

        return $this->connection->transactional(function () use ($now): int {
            $deleted = (int) $this->connection->executeStatement(
                'DELETE FROM account_link WHERE expires_at <= :before',
                ['before' => $now->sub(new \DateInterval(self::KEEP_EXPIRED_LINKS))->format('Y-m-d H:i:s')],
            );

            return $deleted + (int) $this->connection->executeStatement(
                'DELETE FROM app_user u WHERE u.email_confirmed_at IS NULL AND u.created_at < :before
                   AND NOT EXISTS (SELECT 1 FROM account_link l WHERE l.user_id = u.id AND l.expires_at > :now)',
                ['before' => $now->sub(new \DateInterval(self::KEEP_UNCONFIRMED))->format('Y-m-d H:i:s'), 'now' => $now->format('Y-m-d H:i:s')],
            );
        });
    }
}
