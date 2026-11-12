<?php

declare(strict_types=1);

namespace App\Account;

use Doctrine\DBAL\Connection;
use Symfony\Component\Uid\Uuid;

/**
 * Deletes a user and everything they own, at once and for good: their own synced records, their personal organization
 * with its synced records, revisions and membership, then the user row, which the database cascades to their devices
 * and their account links, and with it every OAuth2 token and authorization code issued to them. Nothing of the user
 * is kept outside the database, so nothing is left to purge afterwards.
 *
 * The records go first, on their own: left to the cascade from the user row, the database would also clear the
 * device and author columns of rows it is deleting in the same statement.
 *
 * Not covered: database backups, WAL archives and point-in-time recovery keep the rows until they expire.
 */
final readonly class UserDeletion
{
    public function __construct(private Connection $connection)
    {
    }

    public function delete(Uuid $user): void
    {
        $email = $this->connection->fetchOne('SELECT email FROM app_user WHERE id = :id', ['id' => $user->toRfc4122()]);
        if (!\is_string($email)) {
            throw new \InvalidArgumentException('There is no such user.');
        }

        $this->connection->transactional(function () use ($user, $email): void {
            $this->connection->executeStatement(
                'DELETE FROM oauth2_refresh_token WHERE access_token IN (SELECT identifier FROM oauth2_access_token WHERE user_identifier = :email)',
                ['email' => $email],
            );
            $this->connection->executeStatement('DELETE FROM oauth2_access_token WHERE user_identifier = :email', ['email' => $email]);
            $this->connection->executeStatement('DELETE FROM oauth2_authorization_code WHERE user_identifier = :email', ['email' => $email]);
            $this->connection->executeStatement('DELETE FROM user_record WHERE user_id = :id', ['id' => $user->toRfc4122()]);
            $this->connection->executeStatement('DELETE FROM organization WHERE personal_owner_id = :id', ['id' => $user->toRfc4122()]);
            $this->connection->executeStatement('DELETE FROM app_user WHERE id = :id', ['id' => $user->toRfc4122()]);
        });
    }
}
