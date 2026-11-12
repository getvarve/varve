<?php

declare(strict_types=1);

namespace App\Sync;

use App\Entity\User;
use Doctrine\DBAL\Connection;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;
use Symfony\Component\Uid\Uuid;

/**
 * The scopes a user may sync. For now that is their own records and their personal organization's: teams come with
 * roles and grants (design 8.2 and 8.3), and until then no other organization's records are reachable. Any other
 * organization id, whether it exists or not, is a 404, so ids cannot be probed.
 */
final readonly class SyncScopes
{
    public function __construct(private Connection $connection)
    {
    }

    public function organization(User $user, mixed $organizationId): SyncScope
    {
        if (!\is_string($organizationId) || 1 !== preg_match(PayloadRules::UUID, $organizationId)) {
            throw new NotFoundHttpException('Organization not found.');
        }
        $found = $this->connection->fetchOne(
            'SELECT o.id FROM organization o JOIN organization_member m ON m.organization_id = o.id AND m.user_id = :user
             WHERE o.id = :id AND o.personal_owner_id = :user',
            ['id' => $organizationId, 'user' => $user->getId()->toRfc4122()],
        );
        if (!\is_string($found)) {
            throw new NotFoundHttpException('Organization not found.');
        }

        return SyncScope::organization(Uuid::fromString($found));
    }

    public function user(User $user): SyncScope
    {
        return SyncScope::user($user->getId());
    }
}
