<?php

declare(strict_types=1);

namespace App\Device;

use Doctrine\DBAL\Connection;
use League\Bundle\OAuth2ServerBundle\Security\Exception\OAuth2AuthenticationFailedException;
use League\Bundle\OAuth2ServerBundle\Security\Passport\Badge\ScopeBadge;
use Psr\Clock\ClockInterface;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;
use Symfony\Component\Security\Http\Event\CheckPassportEvent;

/**
 * Lets an access token into the API only while the device it was issued to is signed in, and marks that device seen.
 * Revoking a device's tokens already shuts them out; this holds even for one that a refresh revoked moments ago and
 * that still counts during the rotation grace, and for any token that never got a device.
 */
#[AsEventListener(event: CheckPassportEvent::class, dispatcher: 'security.event_dispatcher.api')]
final readonly class DeviceGuard
{
    /** How stale `last_seen_at` may get before a request writes it again: one write per device per five minutes. */
    public const int SEEN_PRECISION = 300;

    public function __construct(
        private Connection $connection,
        private ClockInterface $clock,
    ) {
    }

    public function __invoke(CheckPassportEvent $event): void
    {
        $passport = $event->getPassport();
        if (!$passport->hasBadge(ScopeBadge::class)) {
            return;
        }
        $accessToken = $passport->getAttribute('accessTokenId');
        $device = \is_string($accessToken) ? $this->connection->fetchAssociative(
            'SELECT d.id, d.last_seen_at FROM device_session s JOIN device d ON d.id = s.device_id WHERE s.access_token = :token',
            ['token' => $accessToken],
        ) : false;
        if (false === $device) {
            throw OAuth2AuthenticationFailedException::create('The device this token was issued to is signed out.');
        }

        $now = $this->clock->now();
        $lastSeen = \is_string($device['last_seen_at']) ? new \DateTimeImmutable($device['last_seen_at']) : null;
        if (null === $lastSeen || $now->getTimestamp() - $lastSeen->getTimestamp() >= self::SEEN_PRECISION) {
            $this->connection->executeStatement('UPDATE device SET last_seen_at = :now WHERE id = :id', ['now' => $now->format('Y-m-d H:i:s'), 'id' => $device['id']]);
        }
    }
}
