<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use Doctrine\DBAL\Connection;
use Psr\Cache\CacheItemPoolInterface;
use Psr\Clock\ClockInterface;
use Symfony\Component\DependencyInjection\Attribute\Autowire;

/**
 * A short grace after a refresh token rotated (OAuth 2.0 Security BCP 4.14.2 allows it for public clients), so one
 * lost response does not end a sign-in.
 *
 * The token endpoint revokes the old refresh token and its access token the moment it issues the new pair. A client
 * whose response never arrived (a reload, a closed lid, a dropped connection) still holds the old ones, and so does a
 * second window that asks with the same token once the first answer went out. For {@see self::GRACE_SECONDS} after
 * the rotation was answered:
 * - the same client presenting the old refresh token (with the same scope) gets the very response already issued for
 *   it, the same new pair, instead of `invalid_grant`; nothing new is issued, so no second token family appears;
 * - the old access token keeps working, so requests already sent with it do not come back 401.
 * Both only while the new refresh token is still live: once it is used, revoked (sign-out, a disconnected app, a
 * deleted user) or gone, the old tokens are dead at once. After the grace, the old refresh token is refused exactly
 * as before.
 *
 * Two requests with the same token at the very same moment are not covered: the rotation is recorded when its response
 * goes out, so a second request that arrives while the first one is still running gets `invalid_grant` (or, before the
 * first revoked anything, a second pair) as without the grace. Clients refresh one at a time (the web app across its
 * tabs); a client with several processes that share a token has to do the same.
 *
 * What a rotation left behind sits in `cache.oauth_rotation` (Redis in production, shared by every API instance) for a
 * little longer than the grace, with the response encrypted with the authorization server's key ({@see TokenCrypt}).
 */
final readonly class RefreshTokenRotation
{
    public const int GRACE_SECONDS = 30;

    /** How long an entry stays in the cache; the grace itself is checked against the clock. */
    private const int KEEP_SECONDS = 120;

    public function __construct(
        #[Autowire(service: 'cache.oauth_rotation')]
        private CacheItemPoolInterface $cache,
        private TokenCrypt $crypt,
        private ClockInterface $clock,
        private Connection $connection,
    ) {
    }

    /**
     * Records that the token endpoint answered a refresh with $response, after which $successor is the new refresh
     * token.
     */
    public function rotated(IssuedToken $old, ?string $scope, string $response, string $successor): void
    {
        $at = $this->clock->now()->getTimestamp();

        $refresh = $this->cache->getItem(self::refreshKey($old->id));
        if ($refresh->isHit()) {
            // Answered once already; the grace never starts again.
            return;
        }
        $refresh->set(['at' => $at, 'client' => $old->clientId, 'scope' => $scope, 'response' => $this->crypt->seal($response), 'successor' => $successor]);
        $refresh->expiresAfter(self::KEEP_SECONDS);
        $this->cache->saveDeferred($refresh);

        $access = $this->cache->getItem(self::accessKey($old->accessTokenId));
        $access->set(['at' => $at, 'successor' => $successor]);
        $access->expiresAfter(self::KEEP_SECONDS);
        $this->cache->saveDeferred($access);

        $this->cache->commit();
    }

    /**
     * The response issued when $old rotated, as JSON, when the same client asks again with the same scope within the
     * grace and the new refresh token is still live; `expires_in` counts from now.
     */
    public function replay(IssuedToken $old, ?string $scope): ?string
    {
        $entry = $this->entry(self::refreshKey($old->id));
        $sealed = $entry['response'] ?? null;
        if (null === $entry || ($entry['client'] ?? null) !== $old->clientId || ($entry['scope'] ?? null) !== $scope || !\is_string($sealed)) {
            return null;
        }
        $response = $this->crypt->open($sealed);
        if (null === $response) {
            return null;
        }

        try {
            $body = json_decode($response, true, 4, \JSON_THROW_ON_ERROR);
        } catch (\JsonException) {
            return null;
        }
        if (!\is_array($body)) {
            return null;
        }
        $expiresIn = $body['expires_in'] ?? null;
        if (\is_int($expiresIn)) {
            $body['expires_in'] = max(0, $expiresIn - ($this->clock->now()->getTimestamp() - $entry['at']));
        }

        return json_encode($body, \JSON_THROW_ON_ERROR | \JSON_UNESCAPED_SLASHES);
    }

    /** Whether an access token that a refresh revoked still counts as valid. */
    public function accessTokenInGrace(string $accessTokenId): bool
    {
        return null !== $this->entry(self::accessKey($accessTokenId));
    }

    /** The refresh token that replaced this one, while the rotation is remembered. */
    public function successorOf(string $refreshTokenId): ?string
    {
        $item = $this->cache->getItem(self::refreshKey($refreshTokenId));
        $value = $item->isHit() ? $item->get() : null;
        $successor = \is_array($value) ? ($value['successor'] ?? null) : null;

        return \is_string($successor) ? $successor : null;
    }

    /** Ends the grace of these tokens at once (explicit revocation). */
    public function forget(?string $refreshTokenId, ?string $accessTokenId): void
    {
        $keys = [];
        if (null !== $refreshTokenId) {
            $keys[] = self::refreshKey($refreshTokenId);
        }
        if (null !== $accessTokenId) {
            $keys[] = self::accessKey($accessTokenId);
        }
        if ([] !== $keys) {
            $this->cache->deleteItems($keys);
        }
    }

    /**
     * An entry still within its grace, whose new refresh token is live.
     *
     * @return array{at: int, successor: string, client?: mixed, scope?: mixed, response?: mixed}|null
     */
    private function entry(string $key): ?array
    {
        $item = $this->cache->getItem($key);
        if (!$item->isHit()) {
            return null;
        }
        $value = $item->get();
        if (!\is_array($value) || !\is_int($value['at'] ?? null) || !\is_string($value['successor'] ?? null)) {
            return null;
        }
        if ($this->clock->now()->getTimestamp() - $value['at'] > self::GRACE_SECONDS || !$this->isLive($value['successor'])) {
            return null;
        }

        /** @var array{at: int, successor: string, client?: mixed, scope?: mixed, response?: mixed} */
        return $value;
    }

    private function isLive(string $refreshTokenId): bool
    {
        return false !== $this->connection->fetchOne(
            'SELECT 1 FROM oauth2_refresh_token WHERE identifier = :id AND revoked = false AND expiry > :now',
            ['id' => $refreshTokenId, 'now' => $this->clock->now()->format('Y-m-d H:i:s')],
        );
    }

    private static function refreshKey(string $refreshTokenId): string
    {
        return 'refresh.'.hash('sha256', $refreshTokenId);
    }

    private static function accessKey(string $accessTokenId): string
    {
        return 'access.'.hash('sha256', $accessTokenId);
    }
}
