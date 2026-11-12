<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProviderInterface;
use App\ApiResource\Me;
use App\Repository\OrganizationMemberRepository;
use App\Security\CurrentUser;

/**
 * @implements ProviderInterface<Me>
 */
final readonly class MeProvider implements ProviderInterface
{
    public function __construct(
        private CurrentUser $currentUser,
        private OrganizationMemberRepository $members,
    ) {
    }

    public function provide(Operation $operation, array $uriVariables = [], array $context = []): Me
    {
        $user = $this->currentUser->get();
        $organization = $this->members->personalOrganizationOf($user);
        $seats = $this->members->countPerOrganization([$organization->getId()])[$organization->getId()->toRfc4122()] ?? 0;

        return Me::fromUser($user, $organization, $seats);
    }
}
