<?php

declare(strict_types=1);

namespace App\Sync;

use Symfony\Component\HttpKernel\Exception\GoneHttpException;

/**
 * The cursor is from before tombstones the client never saw were purged (or from a state of the feed the server does
 * not have, such as before a restore from backup): the client has to pull everything again, without a cursor.
 */
final class CursorExpired extends GoneHttpException
{
    public function __construct()
    {
        parent::__construct('This cursor is too old: deletes after it were purged. Pull everything again without a cursor.');
    }
}
