<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\CollectionOperationInterface;
use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProviderInterface;
use App\ApiResource\DeviceResource;
use App\Device\CurrentDevice;
use App\Entity\Device;
use App\Repository\DeviceRepository;
use App\Security\CurrentUser;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;

/**
 * Only ever returns devices of the signed-in user.
 *
 * @implements ProviderInterface<DeviceResource>
 */
final readonly class DeviceProvider implements ProviderInterface
{
    public function __construct(
        private DeviceRepository $devices,
        private CurrentUser $currentUser,
        private CurrentDevice $currentDevice,
    ) {
    }

    /**
     * @return DeviceResource|list<DeviceResource>
     */
    public function provide(Operation $operation, array $uriVariables = [], array $context = []): DeviceResource|array
    {
        $user = $this->currentUser->get();
        $current = $this->currentDevice->id();
        $resource = static fn (Device $device): DeviceResource => DeviceResource::fromEntity($device, null !== $current && $current->equals($device->getId()));

        if ($operation instanceof CollectionOperationInterface) {
            return array_map($resource, $this->devices->findSignedInOf($user));
        }

        $id = $uriVariables['id'] ?? null;
        $device = \is_string($id) ? $this->devices->findOneSignedInOf($user, $id) : null;
        if (null === $device) {
            throw new NotFoundHttpException('Device not found.');
        }

        return $resource($device);
    }
}
