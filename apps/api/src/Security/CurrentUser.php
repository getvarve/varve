<?php

declare(strict_types=1);

namespace App\Security;

use App\Entity\User;
use Symfony\Bundle\SecurityBundle\Security;
use Symfony\Component\Security\Core\Exception\AccessDeniedException;

/**
 * The user behind the current request.
 */
final readonly class CurrentUser
{
    public function __construct(private Security $security)
    {
    }

    public function get(): User
    {
        $user = $this->security->getUser();
        if (!$user instanceof User) {
            throw new AccessDeniedException('This endpoint requires a user token.');
        }

        return $user;
    }
}
