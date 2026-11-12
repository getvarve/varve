<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use App\Device\Devices;
use League\Bundle\OAuth2ServerBundle\Manager\ClientManagerInterface;
use Psr\Clock\ClockInterface;

/**
 * Token revocation (RFC 7009) for public clients: a client that signs out tells the server to forget its tokens.
 *
 * Every token of ours belongs to a device ({@see \App\Entity\Device}), so revoking one signs that whole device out:
 * all its access and refresh tokens are revoked, including what a refresh rotated into moments ago (so a client
 * signing out with the token whose refresh response it never received still ends everything), and the device leaves
 * the user's list.
 *
 * Only tokens issued to the client that asks, and only for clients without a secret: a confidential client would have
 * to authenticate first, which this endpoint does not do. Whatever is presented, the caller learns nothing about it
 * (RFC 7009 2.2: an unknown, foreign or already revoked token is answered like any other).
 *
 * An access token that expired signs its device out only while the refresh token issued with it can still be
 * exchanged, which is what an app that was closed for a while holds. Once the app refreshed, the old access token is
 * a leftover (in a log, a crash report) and does nothing, however long its device lives on.
 */
final readonly class TokenRevocation
{
    public function __construct(
        private IssuedTokens $tokens,
        private ClientManagerInterface $clients,
        private Devices $devices,
        private ClockInterface $clock,
    ) {
    }

    /**
     * @param string|null $hint `refresh_token` or `access_token` (RFC 7009 2.1); any other value is no hint
     */
    public function revoke(#[\SensitiveParameter] string $token, ?string $hint, string $clientId): void
    {
        $issued = IssuedToken::ACCESS_TOKEN === $hint
            ? ($this->tokens->accessToken($token) ?? $this->tokens->refreshToken($token))
            : ($this->tokens->refreshToken($token) ?? $this->tokens->accessToken($token));
        if (null === $issued || $issued->clientId !== $clientId || !$this->isPublic($clientId) || $this->isLeftover($issued)) {
            return;
        }

        $this->devices->signOutByAccessToken($issued->accessTokenId);
    }

    /** An expired access token whose refresh token was used, revoked or expired since. */
    private function isLeftover(IssuedToken $issued): bool
    {
        if (IssuedToken::ACCESS_TOKEN !== $issued->type || null === $issued->expiresAt || $issued->expiresAt > $this->clock->now()) {
            return false;
        }
        $refreshToken = $this->tokens->refreshTokenIssuedWith($issued->id);

        return null === $refreshToken || !$this->tokens->isRefreshTokenLive($refreshToken);
    }

    private function isPublic(string $clientId): bool
    {
        $client = $this->clients->find($clientId);

        return null !== $client && \in_array($client->getSecret(), [null, ''], true);
    }
}
