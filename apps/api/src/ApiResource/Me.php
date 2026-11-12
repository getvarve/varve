<?php

declare(strict_types=1);

namespace App\ApiResource;

use ApiPlatform\Metadata\ApiProperty;
use ApiPlatform\Metadata\ApiResource;
use ApiPlatform\Metadata\Get;
use App\ApiResource\Output\PersonalOrganization;
use App\Entity\Organization;
use App\Entity\User;
use App\Security\Scope;
use App\State\MeProvider;

/**
 * The signed-in user, with the personal organization they own. Their other organizations (teams, later) are at
 * /api/v1/organizations.
 */
#[ApiResource(
    shortName: 'Me',
    routePrefix: '/api/v1',
    operations: [
        new Get(
            uriTemplate: '/me',
            uriVariables: [],
            security: "is_granted('".Scope::ROLE_ACCOUNT."')",
            provider: MeProvider::class,
        ),
    ],
)]
final readonly class Me
{
    public function __construct(
        #[ApiProperty(identifier: true)]
        public string $id,
        public string $email,
        public string $displayName,
        public \DateTimeImmutable $createdAt,
        public PersonalOrganization $personalOrganization,
    ) {
    }

    public static function fromUser(User $user, Organization $personalOrganization, int $seats): self
    {
        return new self(
            $user->getId()->toRfc4122(),
            $user->getEmail(),
            $user->getDisplayName(),
            $user->getCreatedAt(),
            PersonalOrganization::of($personalOrganization, $seats),
        );
    }
}
