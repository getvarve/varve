<?php

declare(strict_types=1);

namespace App\Device;

use App\Entity\User;
use App\Security\OAuth\RefreshTokenRotation;
use Doctrine\DBAL\ArrayParameterType;
use Doctrine\DBAL\Connection;
use Symfony\Component\Uid\Uuid;

/**
 * Signs devices out: their access and refresh tokens are revoked and the device row goes, which takes its sessions
 * along. The rotation grace of those tokens ends at once ({@see RefreshTokenRotation}), and a refresh token that
 * rotated moments ago takes its successor along, so nothing a device was issued keeps working.
 *
 * Everything here works in SQL, in one transaction per call: tokens are the bundle's rows, not entities of ours.
 */
final readonly class Devices
{
    /** How many rotations in a row are followed; more than one only happens within the grace. */
    private const int MAX_SUCCESSORS = 5;

    public function __construct(
        private Connection $connection,
        private RefreshTokenRotation $rotation,
    ) {
    }

    public function signOut(Uuid $device): void
    {
        $this->connection->transactional(function () use ($device): void {
            $this->signOutDevice($device->toRfc4122());
        });
    }

    /**
     * Signs out the device an access token (or the refresh token issued with it) belongs to. A token without a device
     * (none should exist) is revoked on its own.
     */
    public function signOutByAccessToken(string $accessTokenId): void
    {
        $this->connection->transactional(function () use ($accessTokenId): void {
            $device = $this->connection->fetchOne('SELECT device_id FROM device_session WHERE access_token = :id', ['id' => $accessTokenId]);
            if (\is_string($device)) {
                $this->signOutDevice($device);

                return;
            }
            $this->revokeAccessTokens([$accessTokenId]);
        });
    }

    /**
     * Signs out every device of the user but $keep (the one that asked, when it is one of theirs), and revokes every
     * other token and pending authorization code the user has. After a password change: whoever had the old password
     * is out everywhere else.
     */
    public function signOutOthers(User $user, ?Uuid $keep): void
    {
        $this->connection->transactional(function () use ($user, $keep): void {
            $keepId = $keep?->toRfc4122();
            foreach ($this->devicesOf($user) as $device) {
                if ($device !== $keepId) {
                    $this->signOutDevice($device);
                }
            }

            $kept = null === $keepId ? [] : $this->accessTokensOf($keepId);
            $this->revokeAccessTokens(array_values(array_diff($this->userAccessTokens($user), $kept)));
            $this->connection->executeStatement('UPDATE oauth2_authorization_code SET revoked = true WHERE user_identifier = :user', ['user' => $user->getUserIdentifier()]);
        });
    }

    /** Signs out every device of the user and revokes every token and pending authorization code they have. */
    public function signOutEverywhere(User $user): void
    {
        $this->signOutOthers($user, null);
    }

    private function signOutDevice(string $device): void
    {
        $this->revokeAccessTokens($this->accessTokensOf($device));
        $this->connection->executeStatement('DELETE FROM device WHERE id = :id', ['id' => $device]);
    }

    /**
     * Revokes these access tokens, the refresh tokens issued with them, and whatever those rotated into just now.
     *
     * @param list<string> $accessTokens
     */
    private function revokeAccessTokens(array $accessTokens): void
    {
        $seen = [];
        for ($round = 0; [] !== $accessTokens && $round <= self::MAX_SUCCESSORS; ++$round) {
            $this->connection->executeStatement('UPDATE oauth2_access_token SET revoked = true WHERE identifier IN (:ids)', ['ids' => $accessTokens], ['ids' => ArrayParameterType::STRING]);
            $refreshTokens = self::strings($this->connection->fetchFirstColumn('SELECT identifier FROM oauth2_refresh_token WHERE access_token IN (:ids)', ['ids' => $accessTokens], ['ids' => ArrayParameterType::STRING]));
            if ([] !== $refreshTokens) {
                $this->connection->executeStatement('UPDATE oauth2_refresh_token SET revoked = true WHERE identifier IN (:ids)', ['ids' => $refreshTokens], ['ids' => ArrayParameterType::STRING]);
            }

            $successors = [];
            foreach ($refreshTokens as $refreshToken) {
                $seen[$refreshToken] = true;
                $successor = $this->rotation->successorOf($refreshToken);
                if (null !== $successor && !isset($seen[$successor])) {
                    $successors[] = $successor;
                }
                $this->rotation->forget($refreshToken, null);
            }
            foreach ($accessTokens as $accessToken) {
                $this->rotation->forget(null, $accessToken);
            }

            $accessTokens = [] === $successors ? [] : self::strings($this->connection->fetchFirstColumn(
                'SELECT access_token FROM oauth2_refresh_token WHERE identifier IN (:ids) AND access_token IS NOT NULL',
                ['ids' => $successors],
                ['ids' => ArrayParameterType::STRING],
            ));
            if ([] !== $successors) {
                $this->connection->executeStatement('UPDATE oauth2_refresh_token SET revoked = true WHERE identifier IN (:ids)', ['ids' => $successors], ['ids' => ArrayParameterType::STRING]);
            }
        }
    }

    /** @return list<string> */
    private function accessTokensOf(string $device): array
    {
        return self::strings($this->connection->fetchFirstColumn('SELECT access_token FROM device_session WHERE device_id = :id', ['id' => $device]));
    }

    /** @return list<string> */
    private function devicesOf(User $user): array
    {
        return self::strings($this->connection->fetchFirstColumn('SELECT id FROM device WHERE user_id = :user', ['user' => $user->getId()->toRfc4122()]));
    }

    /**
     * The user's access tokens that still work, or whose refresh token still does.
     *
     * @return list<string>
     */
    private function userAccessTokens(User $user): array
    {
        return self::strings($this->connection->fetchFirstColumn(
            'SELECT a.identifier FROM oauth2_access_token a WHERE a.user_identifier = :user
               AND (a.revoked = false OR EXISTS (SELECT 1 FROM oauth2_refresh_token r WHERE r.access_token = a.identifier AND r.revoked = false))',
            ['user' => $user->getUserIdentifier()],
        ));
    }

    /**
     * @param list<mixed> $values
     *
     * @return list<string>
     */
    private static function strings(array $values): array
    {
        return array_values(array_filter($values, \is_string(...)));
    }
}
