<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use Doctrine\DBAL\Connection;
use Psr\Clock\ClockInterface;
use Symfony\Component\Messenger\Attribute\AsMessageHandler;

/**
 * Every sign-in and every hourly refresh adds an access token and a refresh token; nothing else ever removes them, and
 * the authenticator, signing out devices and deleting a user all search these tables. An expired token or code can
 * never be used again, so it goes.
 *
 * Expired refresh tokens go first. An expired access token goes only when no refresh token is left that was issued
 * with it: signing a device out finds its refresh tokens through their access tokens, and deleting the access token
 * would leave such a refresh token without that link (the foreign key sets it to null). Deleting an access token takes
 * its device session along; a device left without any session has no token that could still work (its last refresh
 * token expired, unused for 30 days), so it goes too.
 */
#[AsMessageHandler]
final readonly class PurgeExpiredOAuthTokensHandler
{
    public function __construct(
        private Connection $connection,
        private ClockInterface $clock,
    ) {
    }

    /** @return int how many rows were deleted, of all four kinds together */
    public function __invoke(PurgeExpiredOAuthTokens $message): int
    {
        $now = $this->clock->now()->format('Y-m-d H:i:s');

        return $this->connection->transactional(function () use ($now): int {
            $deleted = (int) $this->connection->executeStatement('DELETE FROM oauth2_refresh_token WHERE expiry < :now', ['now' => $now]);
            $deleted += (int) $this->connection->executeStatement(
                'DELETE FROM oauth2_access_token a WHERE a.expiry < :now
                   AND NOT EXISTS (SELECT 1 FROM oauth2_refresh_token r WHERE r.access_token = a.identifier)',
                ['now' => $now],
            );
            $deleted += (int) $this->connection->executeStatement('DELETE FROM oauth2_authorization_code WHERE expiry < :now', ['now' => $now]);

            return $deleted + (int) $this->connection->executeStatement('DELETE FROM device d WHERE NOT EXISTS (SELECT 1 FROM device_session s WHERE s.device_id = d.id)');
        });
    }
}
