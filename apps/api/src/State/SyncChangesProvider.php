<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProviderInterface;
use App\Security\CurrentUser;
use App\Sync\Cursor;
use App\Sync\RecordPull;
use App\Sync\SyncLimits;
use App\Sync\SyncScopes;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Request;

/**
 * Reads a page of the signed-in user's own records, or of their personal organization's, after a cursor.
 *
 * @implements ProviderInterface<JsonResponse>
 */
final readonly class SyncChangesProvider implements ProviderInterface
{
    public function __construct(
        private CurrentUser $currentUser,
        private SyncScopes $scopes,
        private SyncLimits $limits,
        private RecordPull $pull,
    ) {
    }

    public function provide(Operation $operation, array $uriVariables = [], array $context = []): JsonResponse
    {
        $user = $this->currentUser->get();
        $request = $context['request'] ?? null;
        if (!$request instanceof Request) {
            throw new \LogicException('A pull needs its request.');
        }
        $this->limits->pullBy($user);
        $scope = 'sync_changes_user' === $operation->getName()
            ? $this->scopes->user($user)
            : $this->scopes->organization($user, $request->attributes->get('organizationId'));
        $this->limits->pullFrom($scope);

        // Checked here as well as by the parameters of the operation, so the answer does not depend on those: a
        // parameter can also arrive as a list (?cursor[]=x).
        $query = $request->query->all();
        $cursor = null;
        if (\array_key_exists('cursor', $query)) {
            $cursor = \is_string($query['cursor']) ? Cursor::decode($query['cursor'], $scope) : null;
            if (null === $cursor) {
                throw Violation::on('cursor', 'This is not a cursor of these records.');
            }
        }
        $limit = $query['limit'] ?? (string) RecordPull::DEFAULT_LIMIT;
        if (!\is_string($limit) || 1 !== preg_match('/^\d{1,4}$/', $limit) || (int) $limit < 1 || (int) $limit > RecordPull::MAX_LIMIT) {
            throw Violation::on('limit', \sprintf('limit must be a whole number from 1 to %d.', RecordPull::MAX_LIMIT));
        }

        return SyncJson::response($this->pull->pull($scope, $cursor, (int) $limit));
    }
}
