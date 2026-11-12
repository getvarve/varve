<?php

declare(strict_types=1);

namespace App\Security;

/**
 * OAuth2 scopes. Keep in sync with config/packages/league_oauth2_server.yaml.
 *
 * Only our own apps sign in, and they ask for both. The split is there so a later kind of client (a script, a
 * read-only view) can get less than everything.
 */
enum Scope: string
{
    /** The account itself: who the user is, their organizations and devices. */
    case Account = 'account';
    /** The synced records: connection profiles, folders, saved queries and the rest of design 8.6. */
    case Sync = 'sync';

    /** Role granted by the OAuth2 authenticator for this scope (see `role_prefix`). */
    public const string ROLE_PREFIX = 'ROLE_OAUTH2_';

    public const string ROLE_ACCOUNT = 'ROLE_OAUTH2_ACCOUNT';
    public const string ROLE_SYNC = 'ROLE_OAUTH2_SYNC';

    public function role(): string
    {
        return self::ROLE_PREFIX.strtoupper($this->value);
    }

    /** @return list<string> */
    public static function values(): array
    {
        return array_map(static fn (self $scope): string => $scope->value, self::cases());
    }
}
