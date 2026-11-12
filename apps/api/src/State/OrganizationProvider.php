<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProviderInterface;
use App\ApiResource\OrganizationResource;
use App\Entity\OrganizationMember;
use App\Repository\OrganizationMemberRepository;
use App\Security\CurrentUser;

/**
 * Only ever returns organizations the signed-in user is a member of.
 *
 * @implements ProviderInterface<OrganizationResource>
 */
final readonly class OrganizationProvider implements ProviderInterface
{
    public function __construct(
        private OrganizationMemberRepository $members,
        private CurrentUser $currentUser,
    ) {
    }

    /**
     * @return list<OrganizationResource>
     */
    public function provide(Operation $operation, array $uriVariables = [], array $context = []): array
    {
        $memberships = $this->members->findOfUser($this->currentUser->get());
        $seats = $this->members->countPerOrganization(
            array_map(static fn (OrganizationMember $membership) => $membership->getOrganization()->getId(), $memberships),
        );

        return array_map(
            static fn (OrganizationMember $membership): OrganizationResource => OrganizationResource::fromMembership(
                $membership,
                $seats[$membership->getOrganization()->getId()->toRfc4122()] ?? 0,
            ),
            $memberships,
        );
    }
}
