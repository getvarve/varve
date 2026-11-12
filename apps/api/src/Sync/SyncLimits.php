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

    /**
     * Counts a push against its user. Called before the organization in the address is looked up, so requests for
     * organizations that are not the user's count as well.
     *
     * @throws TooManyRequestsHttpException
     */
    public function pushBy(User $user): void
    {
        $this->consume($this->pushPerUser, $user->getId()->toRfc4122());
    }

    /**
     * Counts a push against its organization; a user's own records have the limit per user only.
     *
     * @throws TooManyRequestsHttpException
     */
    public function pushTo(SyncScope $scope): void
    {
        if ($scope->isOrganization()) {
            $this->consume($this->pushPerOrganization, $scope->owner->toRfc4122());
        }
    }

    /**
     * Counts a pull against its user, as {@see self::pushBy()} does a push.
     *
     * @throws TooManyRequestsHttpException
     */
    public function pullBy(User $user): void
    {
        $this->consume($this->pullPerUser, $user->getId()->toRfc4122());
    }

    /** @throws TooManyRequestsHttpException */
    public function pullFrom(SyncScope $scope): void
    {
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
