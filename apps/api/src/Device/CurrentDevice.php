<?php

declare(strict_types=1);

namespace App\Device;

use Doctrine\DBAL\Connection;
use League\Bundle\OAuth2ServerBundle\Security\Authentication\Token\OAuth2Token;
use Symfony\Bundle\SecurityBundle\Security;
use Symfony\Component\Uid\Uuid;

/**
 * The device the current request comes from: the one its access token was issued to.
 */
final readonly class CurrentDevice
{
    public function __construct(
        private Security $security,
        private Connection $connection,
    ) {
    }

    public function id(): ?Uuid
    {
        $token = $this->security->getToken();
        if (!$token instanceof OAuth2Token) {
            return null;
        }
        $device = $this->connection->fetchOne('SELECT device_id FROM device_session WHERE access_token = :token', ['token' => $token->getCredentials()]);

        return \is_string($device) ? Uuid::fromString($device) : null;
    }
}
