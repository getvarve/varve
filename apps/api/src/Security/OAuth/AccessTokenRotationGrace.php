<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use League\OAuth2\Server\Entities\AccessTokenEntityInterface;
use League\OAuth2\Server\Entities\ClientEntityInterface;
use League\OAuth2\Server\Repositories\AccessTokenRepositoryInterface;
use Symfony\Component\DependencyInjection\Attribute\AsDecorator;
use Symfony\Component\DependencyInjection\Attribute\AutowireDecorated;

/**
 * The bundle's access token repository, except that an access token a refresh revoked moments ago still counts as
 * valid during the grace of {@see RefreshTokenRotation}, so requests already sent with it do not come back 401. Every
 * other revocation (explicit, a disconnected app, a deleted user) ends that grace, and a token that was never valid
 * is never made valid.
 */
#[AsDecorator('league.oauth2_server.repository.access_token')]
final readonly class AccessTokenRotationGrace implements AccessTokenRepositoryInterface
{
    public function __construct(
        #[AutowireDecorated]
        private AccessTokenRepositoryInterface $inner,
        private RefreshTokenRotation $rotation,
    ) {
    }

    public function getNewToken(ClientEntityInterface $clientEntity, array $scopes, ?string $userIdentifier = null): AccessTokenEntityInterface
    {
        return $this->inner->getNewToken($clientEntity, $scopes, $userIdentifier);
    }

    public function persistNewAccessToken(AccessTokenEntityInterface $accessTokenEntity): void
    {
        $this->inner->persistNewAccessToken($accessTokenEntity);
    }

    public function revokeAccessToken(string $tokenId): void
    {
        $this->inner->revokeAccessToken($tokenId);
    }

    public function isAccessTokenRevoked(string $tokenId): bool
    {
        return $this->inner->isAccessTokenRevoked($tokenId) && !$this->rotation->accessTokenInGrace($tokenId);
    }
}
