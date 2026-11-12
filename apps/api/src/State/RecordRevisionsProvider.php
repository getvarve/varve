<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProviderInterface;
use App\Security\CurrentUser;
use App\Sync\PayloadRules;
use App\Sync\RecordRows;
use App\Sync\SyncLimits;
use App\Sync\SyncScopes;
use Doctrine\DBAL\Connection;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;

/**
 * The kept revisions of a record of the signed-in user's personal organization, newest first. Counts as a pull for the
 * rate limits.
 *
 * @implements ProviderInterface<JsonResponse>
 */
final readonly class RecordRevisionsProvider implements ProviderInterface
{
    public function __construct(
        private CurrentUser $currentUser,
        private SyncScopes $scopes,
        private SyncLimits $limits,
        private Connection $connection,
    ) {
    }

    public function provide(Operation $operation, array $uriVariables = [], array $context = []): JsonResponse
    {
        $user = $this->currentUser->get();
        $request = $context['request'] ?? null;
        if (!$request instanceof Request) {
            throw new \LogicException('Revisions need their request.');
        }
        $this->limits->pullBy($user);
        $scope = $this->scopes->organization($user, $request->attributes->get('organizationId'));
        $this->limits->pullFrom($scope);

        $record = $request->attributes->get('recordId');
        $parameters = ['organization' => $scope->owner->toRfc4122(), 'record' => $record];
        if (!\is_string($record) || 1 !== preg_match(PayloadRules::UUID, $record)
            || false === $this->connection->fetchOne('SELECT 1 FROM record WHERE organization_id = :organization AND id = :record', $parameters)) {
            throw new NotFoundHttpException('Record not found.');
        }

        $rows = $this->connection->fetchAllAssociative(
            'SELECT version, payload::text AS payload, updated_at, updated_by_id, updated_device_id FROM record_revision
             WHERE organization_id = :organization AND record_id = :record ORDER BY version DESC',
            $parameters,
        );

        return SyncJson::response(['revisions' => array_map(static fn (array $row): array => [
            'version' => RecordRows::int($row['version']),
            'payload' => json_decode(\is_string($row['payload']) ? $row['payload'] : 'null', false, 512, \JSON_THROW_ON_ERROR),
            'updatedAt' => RecordRows::date($row['updated_at']),
            'updatedBy' => \is_string($row['updated_by_id']) ? $row['updated_by_id'] : null,
            'updatedByDevice' => \is_string($row['updated_device_id']) ? $row['updated_device_id'] : null,
        ], $rows)]);
    }
}
