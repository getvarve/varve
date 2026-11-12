<?php

declare(strict_types=1);

namespace App\Account;

/**
 * Deletes expired account links, and accounts whose address was never confirmed a week after they were made. Sent by
 * the schedule once a day.
 */
final readonly class PurgeAbandonedSignUps
{
}
