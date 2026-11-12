<?php

declare(strict_types=1);

namespace App\Entity;

use App\Enum\DevicePlatform;
use App\Repository\DeviceRepository;
use Doctrine\ORM\Mapping as ORM;
use League\Bundle\OAuth2ServerBundle\Model\Client;
use Symfony\Bridge\Doctrine\Types\UuidType;
use Symfony\Component\Uid\Uuid;

/**
 * One sign-in of one of our apps: the desktop app on a computer, or the web client in a browser. It starts when the app
 * exchanges its authorization code (the app names itself then) and lasts while it refreshes its tokens; every access
 * token issued on the way belongs to it ({@see DeviceSession}). Signing it out, from the app itself or from another
 * device, revokes its tokens and removes it.
 */
#[ORM\Entity(repositoryClass: DeviceRepository::class)]
#[ORM\Index(columns: ['user_id'])]
#[ORM\Index(columns: ['client'])]
class Device
{
    public const int MAX_NAME = 100;
    public const int MAX_APP_VERSION = 32;

    #[ORM\Id]
    #[ORM\Column(type: UuidType::NAME)]
    private Uuid $id;

    #[ORM\ManyToOne]
    #[ORM\JoinColumn(nullable: false, onDelete: 'CASCADE')]
    private User $user;

    /** The app that signed in (an OAuth2 client of ours). */
    #[ORM\ManyToOne(targetEntity: Client::class)]
    #[ORM\JoinColumn(name: 'client', referencedColumnName: 'identifier', nullable: false, onDelete: 'CASCADE')]
    private Client $client;

    /** "MacBook Pro"; the app proposes it, the user may change it. */
    #[ORM\Column(length: self::MAX_NAME)]
    private string $name;

    #[ORM\Column(length: 16, enumType: DevicePlatform::class)]
    private DevicePlatform $platform;

    /** The version of the app, as it last said. */
    #[ORM\Column(length: self::MAX_APP_VERSION)]
    private string $appVersion;

    #[ORM\Column]
    private \DateTimeImmutable $createdAt;

    /** The last sign-in, token refresh or API request, to within {@see \App\Device\DeviceGuard::SEEN_PRECISION}. */
    #[ORM\Column]
    private \DateTimeImmutable $lastSeenAt;

    public function __construct(User $user, Client $client, string $name, DevicePlatform $platform, string $appVersion, \DateTimeImmutable $createdAt)
    {
        $this->id = Uuid::v7();
        $this->user = $user;
        $this->client = $client;
        $this->name = $name;
        $this->platform = $platform;
        $this->appVersion = $appVersion;
        $this->createdAt = $createdAt;
        $this->lastSeenAt = $createdAt;
    }

    public function getId(): Uuid
    {
        return $this->id;
    }

    public function getUser(): User
    {
        return $this->user;
    }

    public function getClient(): Client
    {
        return $this->client;
    }

    public function getName(): string
    {
        return $this->name;
    }

    public function rename(string $name): void
    {
        $this->name = $name;
    }

    public function getPlatform(): DevicePlatform
    {
        return $this->platform;
    }

    public function getAppVersion(): string
    {
        return $this->appVersion;
    }

    public function getCreatedAt(): \DateTimeImmutable
    {
        return $this->createdAt;
    }

    public function getLastSeenAt(): \DateTimeImmutable
    {
        return $this->lastSeenAt;
    }
}
