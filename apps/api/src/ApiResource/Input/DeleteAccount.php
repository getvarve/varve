<?php

declare(strict_types=1);

namespace App\ApiResource\Input;

use Symfony\Component\Validator\Constraints as Assert;

final class DeleteAccount
{
    /** The current password: deleting cannot be undone, so a token alone is not enough. */
    #[Assert\NotBlank]
    public string $password = '';
}
