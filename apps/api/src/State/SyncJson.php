<?php

declare(strict_types=1);

namespace App\State;

use Symfony\Component\HttpFoundation\JsonResponse;

/**
 * Sync answers are encoded here rather than by the serializer, so record payloads (decoded with objects as \stdClass)
 * come out as they were pushed: `{}` stays an object and 1.0 stays a float.
 */
final class SyncJson
{
    /** @param array<string, mixed> $data */
    public static function response(array $data): JsonResponse
    {
        return JsonResponse::fromJsonString(json_encode($data, \JSON_THROW_ON_ERROR | \JSON_UNESCAPED_SLASHES | \JSON_UNESCAPED_UNICODE | \JSON_PRESERVE_ZERO_FRACTION));
    }
}
