<?php

declare(strict_types=1);

namespace App\Entity;

use Doctrine\ORM\Mapping as ORM;
use League\Bundle\OAuth2ServerBundle\Model\AccessToken;

/**
 * Which device an access token was issued to: one row for the token of the sign-in and one for every refresh after it.
 * The refresh token issued with an access token belongs to the same device through it. The row goes with its access
 * token (the nightly purge of expired tokens) or with its device (signing out).
 */
#[ORM\Entity]
#[ORM\Index(columns: ['device_id'])]
class DeviceSession
{
    #[ORM\Id]
    #[ORM\OneToOne(targetEntity: AccessToken::class)]
    #[ORM\JoinColumn(name: 'access_token', referencedColumnName: 'identifier', nullable: false, onDelete: 'CASCADE')]
    private AccessToken $accessToken;

    #[ORM\ManyToOne]
    #[ORM\JoinColumn(nullable: false, onDelete: 'CASCADE')]
    private Device $device;

    #[ORM\Column]
    private \DateTimeImmutable $createdAt;

    public function __construct(AccessToken $accessToken, Device $device, \DateTimeImmutable $createdAt)
    {
        $this->accessToken = $accessToken;
        $this->device = $device;
        $this->createdAt = $createdAt;
    }

    public function getAccessToken(): AccessToken
    {
        return $this->accessToken;
    }

    public function getDevice(): Device
    {
        return $this->device;
    }

    public function getCreatedAt(): \DateTimeImmutable
    {
        return $this->createdAt;
    }
}
