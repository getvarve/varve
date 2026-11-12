<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Validator\Exception\ValidationException;
use Symfony\Component\Validator\ConstraintViolation;
use Symfony\Component\Validator\ConstraintViolationList;

/**
 * A 422 about one field of the input, in the same shape as the validator's.
 */
final class Violation
{
    public static function on(string $field, string $message): ValidationException
    {
        return new ValidationException(new ConstraintViolationList([new ConstraintViolation($message, null, [], null, $field, null)]));
    }
}
