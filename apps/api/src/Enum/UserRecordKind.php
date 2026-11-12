<?php

declare(strict_types=1);

namespace App\Enum;

/**
 * The kinds of per-user records (design 8.1 and 8.6): they belong to the account, not to an organization, and only
 * that account sees them.
 */
enum UserRecordKind: string
{
    /** A record the user marked as a favourite. */
    case Favourite = 'favourite';
    /** UI preferences and editor settings. */
    case Preference = 'preference';
    /** Fields of a connection profile the user overrides for themselves (design 8.4). */
    case Override = 'override';
}
