<?php

declare(strict_types=1);

namespace App\Sync;

/**
 * Deletes tombstones of synced records 90 days after the delete (design 8.8). Sent by the schedule once a day.
 */
final readonly class PurgeSyncTombstones
{
}
