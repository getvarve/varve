<?php

declare(strict_types=1);

namespace App\Account;

/**
 * What became of an attempt to confirm an address ({@see EmailConfirmation}).
 */
enum ConfirmationResult
{
    /** The address is confirmed and the account has the name and password of the link's sign-up. */
    case Confirmed;
    /** The password is not the one chosen at the sign-up the link belongs to; nothing changed, the link still works. */
    case WrongPassword;
    /** The link does not work (any more). */
    case LinkEnded;
}
