<?php

declare(strict_types=1);

namespace App\ApiResource\Output;

use App\Entity\Organization;

/**
 * The personal organization the signed-in user owns, as /me shows it.
 */
final readonly class PersonalOrganization
{
    public function __construct(
        public string $id,
        /** It goes by its owner's display name. */
        public string $name,
        /** Its members, which are its seats: the owner alone. */
        public int $seats,
        public \DateTimeImmutable $createdAt,
    ) {
    }

    public static function of(Organization $organization, int $seats): self
    {
        return new self($organization->getId()->toRfc4122(), $organization->getName(), $seats, $organization->getCreatedAt());
    }
}
