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
        $scope = 'sync_changes_user' === $operation->getName()
            ? $this->scopes->user($user)
            : $this->scopes->organization($user, $request->attributes->get('organizationId'));
        $this->limits->pull($user, $scope);

        $cursor = null;
        $text = $request->query->get('cursor');
        if (null !== $text) {
            $cursor = Cursor::decode((string) $text, $scope);
            if (null === $cursor) {
                throw Violation::on('cursor', 'This is not a cursor of these records.');
            }
        }

        return SyncJson::response($this->pull->pull($scope, $cursor, (int) $request->query->get('limit', (string) RecordPull::DEFAULT_LIMIT)));
    }
}
