<?php

declare(strict_types=1);

namespace App\ApiResource;

use ApiPlatform\Metadata\ApiProperty;
use ApiPlatform\Metadata\ApiResource;
use ApiPlatform\Metadata\Delete;
use ApiPlatform\Metadata\GetCollection;
use ApiPlatform\Metadata\Patch;
use App\ApiResource\Input\RenameDevice;
use App\Entity\Device;
use App\Enum\DevicePlatform;
use App\Security\FirstPartyClients;
use App\Security\Scope;
use App\State\DeviceProcessor;
use App\State\DeviceProvider;

/**
 * A device the signed-in user is signed in on: the desktop app on a computer, or the web client in a browser. Renaming
 * changes only the name; deleting signs the device out (its tokens stop working at once). Another user's device is a
 * 404. Only our own apps may list, rename or sign out devices.
 */
#[ApiResource(
    shortName: 'Device',
    routePrefix: '/api/v1',
    operations: [
        new GetCollection(
            uriTemplate: '/devices',
            paginationEnabled: false,
            security: "is_granted('".Scope::ROLE_ACCOUNT."') and is_granted('".FirstPartyClients::ATTRIBUTE."')",
            provider: DeviceProvider::class,
        ),
        new Patch(
            uriTemplate: '/devices/{id}',
            security: "is_granted('".Scope::ROLE_ACCOUNT."') and is_granted('".FirstPartyClients::ATTRIBUTE."')",
            input: RenameDevice::class,
            provider: DeviceProvider::class,
            processor: DeviceProcessor::class,
        ),
        new Delete(
            uriTemplate: '/devices/{id}',
            security: "is_granted('".Scope::ROLE_ACCOUNT."') and is_granted('".FirstPartyClients::ATTRIBUTE."')",
            provider: DeviceProvider::class,
            processor: DeviceProcessor::class,
        ),
    ],
)]
final readonly class DeviceResource
{
    public function __construct(
        #[ApiProperty(identifier: true)]
        public string $id,
        public string $name,
        public DevicePlatform $platform,
        /** Which of our apps: `desktop` or `web`. */
        public string $app,
        public string $appVersion,
        /** Whether this is the device the request comes from. */
        public bool $current,
        /** When the device signed in. */
        public \DateTimeImmutable $createdAt,
        /** The last sign-in, token refresh or request, to within five minutes. */
        public \DateTimeImmutable $lastSeenAt,
    ) {
    }

    public static function fromEntity(Device $device, bool $current): self
    {
        return new self(
            $device->getId()->toRfc4122(),
            $device->getName(),
            $device->getPlatform(),
            $device->getClient()->getIdentifier(),
            $device->getAppVersion(),
            $current,
            $device->getCreatedAt(),
            $device->getLastSeenAt(),
        );
    }
}
