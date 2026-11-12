<?php

declare(strict_types=1);

namespace App\Security\OAuth;

/**
 * A token this server issued, as {@see IssuedTokens} read it: its kind, its id, the client it was issued to, the
 * access token it belongs to (an access token's own id; for a refresh token, the access token issued with it), the
 * user it acts for (none for a client's own token) and its scopes.
 */
final readonly class IssuedToken
{
    public const string ACCESS_TOKEN = 'access_token';
    public const string REFRESH_TOKEN = 'refresh_token';

    /**
     * @param list<string> $scopes
     */
    public function __construct(
        public string $type,
        public string $id,
        public string $clientId,
        public string $accessTokenId,
        public ?string $userIdentifier,
        public array $scopes,
    ) {
    }

    /** @return list<string> */
    public static function scopesOf(mixed $scopes): array
    {
        return \is_array($scopes) ? array_values(array_filter($scopes, \is_string(...))) : [];
    }

    public static function userOf(mixed $user): ?string
    {
        return \is_string($user) || \is_int($user) ? (string) $user : null;
    }
}
