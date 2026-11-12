<?php

declare(strict_types=1);

namespace App\Account;

use Symfony\Component\DependencyInjection\Attribute\Autowire;

/**
 * Whether anyone may sign up on this server. Open unless VARVE_SIGN_UP is 0: an operator who closes it adds users
 * with `bin/console app:users:create`, and the sign-up page and its confirmation links say that sign-up is closed.
 */
final readonly class SignUpSettings
{
    public function __construct(
        #[Autowire(env: 'bool:VARVE_SIGN_UP')]
        public bool $open,
    ) {
    }
}
