<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\DeleteOperationInterface;
use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProcessorInterface;
use App\ApiResource\DeviceResource;
use App\ApiResource\Input\RenameDevice;
use App\Device\CurrentDevice;
use App\Device\Devices;
use App\Repository\DeviceRepository;
use App\Security\CurrentUser;
use Doctrine\ORM\EntityManagerInterface;
use Symfony\Component\HttpKernel\Exception\NotFoundHttpException;

/**
 * Renames one of the signed-in user's devices, or signs it out.
 *
 * @implements ProcessorInterface<RenameDevice|DeviceResource, DeviceResource|null>
 */
final readonly class DeviceProcessor implements ProcessorInterface
{
    public function __construct(
        private DeviceRepository $deviceRepository,
        private Devices $devices,
        private CurrentUser $currentUser,
        private CurrentDevice $currentDevice,
        private EntityManagerInterface $entityManager,
    ) {
    }

    public function process(mixed $data, Operation $operation, array $uriVariables = [], array $context = []): ?DeviceResource
    {
        $id = $uriVariables['id'] ?? null;
        $device = (\is_string($id) ? $this->deviceRepository->findOneSignedInOf($this->currentUser->get(), $id) : null)
            ?? throw new NotFoundHttpException('Device not found.');

        if ($operation instanceof DeleteOperationInterface) {
            $this->devices->signOut($device->getId());

            return null;
        }

        if (!$data instanceof RenameDevice) {
            throw new \LogicException(\sprintf('Unexpected input %s.', get_debug_type($data)));
        }
        $device->rename(trim($data->name));
        $this->entityManager->flush();
        $current = $this->currentDevice->id();

        return DeviceResource::fromEntity($device, null !== $current && $current->equals($device->getId()));
    }
}
