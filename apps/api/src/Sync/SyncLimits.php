<?php

declare(strict_types=1);

namespace App\Sync;

use App\Entity\User;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\HttpKernel\Exception\TooManyRequestsHttpException;
use Symfony\Component\RateLimiter\RateLimiterFactoryInterface;

/**
 * Rate limits on sync (design 8.8), per user over all their scopes and per organization over all its members, so one
 * runaway client cannot load the database. The limits (config/packages/rate_limiter.yaml) leave room for a first pull
 * of a large organization page by page.
 */
final readonly class SyncLimits
{
    public function __construct(
        #[Autowire(service: 'limiter.sync_push_user')]
        private RateLimiterFactoryInterface $pushPerUser,
        #[Autowire(service: 'limiter.sync_push_organization')]
        private RateLimiterFactoryInterface $pushPerOrganization,
        #[Autowire(service: 'limiter.sync_pull_user')]
        private RateLimiterFactoryInterface $pullPerUser,
        #[Autowire(service: 'limiter.sync_pull_organization')]
        private RateLimiterFactoryInterface $pullPerOrganization,
    ) {
    }

    /** @throws TooManyRequestsHttpException */
    public function push(User $user, SyncScope $scope): void
    {
        $this->consume($this->pushPerUser, $user->getId()->toRfc4122());
        if ($scope->isOrganization()) {
            $this->consume($this->pushPerOrganization, $scope->owner->toRfc4122());
        }
    }

    /** @throws TooManyRequestsHttpException */
    public function pull(User $user, SyncScope $scope): void
    {
        $this->consume($this->pullPerUser, $user->getId()->toRfc4122());
        if ($scope->isOrganization()) {
            $this->consume($this->pullPerOrganization, $scope->owner->toRfc4122());
        }
    }

    private function consume(RateLimiterFactoryInterface $limiter, string $key): void
    {
        $limit = $limiter->create($key)->consume();
        if (!$limit->isAccepted()) {
            throw new TooManyRequestsHttpException(max(1, $limit->getRetryAfter()->getTimestamp() - time()), 'Too many sync requests. Try again later.');
        }
    }
}
