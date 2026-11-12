<?php

declare(strict_types=1);

namespace App;

use App\Account\PurgeAbandonedSignUps;
use App\Security\OAuth\PurgeExpiredOAuthTokens;
use App\Sync\PurgeSyncTombstones;
use Symfony\Component\Scheduler\Attribute\AsSchedule;
use Symfony\Component\Scheduler\RecurringMessage;
use Symfony\Component\Scheduler\Schedule as SymfonySchedule;
use Symfony\Component\Scheduler\ScheduleProviderInterface;
use Symfony\Contracts\Cache\CacheInterface;

/**
 * Recurring work, sent to the `scheduler_default` transport, which a worker consumes on its own
 * (`bin/console messenger:consume scheduler_default`).
 */
#[AsSchedule]
final readonly class Schedule implements ScheduleProviderInterface
{
    public function __construct(private CacheInterface $cache)
    {
    }

    public function getSchedule(): SymfonySchedule
    {
        return new SymfonySchedule()
            ->stateful($this->cache)
            ->processOnlyLastMissedRun(true)
            ->add(RecurringMessage::cron('47 3 * * *', new PurgeAbandonedSignUps()))
            ->add(RecurringMessage::cron('53 3 * * *', new PurgeExpiredOAuthTokens()))
            ->add(RecurringMessage::cron('59 3 * * *', new PurgeSyncTombstones()));
    }
}
