<?php

declare(strict_types=1);

namespace App\Repository;

use App\Entity\Device;
use App\Entity\User;
use Doctrine\Bundle\DoctrineBundle\Repository\ServiceEntityRepository;
use Doctrine\Persistence\ManagerRegistry;
use Psr\Clock\ClockInterface;
use Symfony\Component\Uid\Uuid;

/**
 * @extends ServiceEntityRepository<Device>
 */
class DeviceRepository extends ServiceEntityRepository
{
    /** A device that still holds a refresh token that works: signed in. */
    private const string SIGNED_IN = 'EXISTS (SELECT 1 FROM device_session s JOIN oauth2_refresh_token r ON r.access_token = s.access_token
        WHERE s.device_id = d.id AND r.revoked = false AND r.expiry > :now)';

    public function __construct(
        ManagerRegistry $registry,
        private readonly ClockInterface $clock,
    ) {
        parent::__construct($registry, Device::class);
    }

    /**
     * The user's signed-in devices, the one seen last first. A device whose refresh token expired (unused for 30 days)
     * is no longer listed; the nightly purge removes it.
     *
     * @return list<Device>
     */
    public function findSignedInOf(User $user): array
    {
        $ids = $this->getEntityManager()->getConnection()->fetchFirstColumn(
            'SELECT d.id FROM device d WHERE d.user_id = :user AND '.self::SIGNED_IN.' ORDER BY d.last_seen_at DESC, d.id DESC',
            ['user' => $user->getId()->toRfc4122(), 'now' => $this->now()],
        );

        // A user has a handful of devices.
        return array_values(array_filter(array_map(
            fn (mixed $id): ?Device => \is_string($id) ? $this->find(Uuid::fromString($id)) : null,
            $ids,
        )));
    }

    /** One of the user's signed-in devices; null for another user's device, a signed-out one or an id that is no uuid. */
    public function findOneSignedInOf(User $user, string $id): ?Device
    {
        if (!Uuid::isValid($id)) {
            return null;
        }
        $found = $this->getEntityManager()->getConnection()->fetchOne(
            'SELECT d.id FROM device d WHERE d.id = :id AND d.user_id = :user AND '.self::SIGNED_IN,
            ['id' => Uuid::fromString($id)->toRfc4122(), 'user' => $user->getId()->toRfc4122(), 'now' => $this->now()],
        );

        return \is_string($found) ? $this->find(Uuid::fromString($found)) : null;
    }

    private function now(): string
    {
        return $this->clock->now()->format('Y-m-d H:i:s');
    }
}
