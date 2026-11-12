<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProcessorInterface;
use App\Device\CurrentDevice;
use App\Security\CurrentUser;
use App\Sync\ChangeParser;
use App\Sync\RecordPush;
use App\Sync\SyncLimits;
use App\Sync\SyncScopes;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpKernel\Exception\UnsupportedMediaTypeHttpException;

/**
 * Applies a push to the signed-in user's own records or to their personal organization's.
 *
 * @implements ProcessorInterface<null, JsonResponse>
 */
final readonly class SyncPushProcessor implements ProcessorInterface
{
    public function __construct(
        private CurrentUser $currentUser,
        private CurrentDevice $currentDevice,
        private SyncScopes $scopes,
        private SyncLimits $limits,
        private RecordPush $push,
    ) {
    }

    public function process(mixed $data, Operation $operation, array $uriVariables = [], array $context = []): JsonResponse
    {
        $user = $this->currentUser->get();
        $scope = 'sync_push_user' === $operation->getName()
            ? $this->scopes->user($user)
            : $this->scopes->organization($user, $uriVariables['organizationId'] ?? null);
        $this->limits->push($user, $scope);

        $request = $context['request'] ?? null;
        if (!$request instanceof Request) {
            throw new \LogicException('A push needs its request.');
        }
        if ('json' !== $request->getContentTypeFormat()) {
            throw new UnsupportedMediaTypeHttpException('A push is sent as application/json.');
        }
        $changes = ChangeParser::parse($request->getContent(), $scope);

        return SyncJson::response(['results' => $this->push->push($scope, $changes, $user->getId(), $this->currentDevice->id())]);
    }
}
