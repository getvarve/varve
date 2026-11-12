<?php

declare(strict_types=1);

namespace App\ApiResource\Input;

use App\Entity\Device;
use Symfony\Component\Validator\Constraints as Assert;

final class RenameDevice
{
    #[Assert\NotBlank(normalizer: 'trim')]
    #[Assert\Length(max: Device::MAX_NAME, normalizer: 'trim')]
    #[Assert\Regex(pattern: '/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/u', match: false, message: 'The name cannot hold control characters.')]
    public string $name = '';
}
