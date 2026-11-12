<?php

declare(strict_types=1);

namespace App\ApiResource;

use ApiPlatform\Metadata\ApiResource;
use ApiPlatform\Metadata\Get;
use ApiPlatform\Metadata\QueryParameter;
use App\Security\Scope;
use App\State\SyncChangesProvider;
use App\Sync\RecordPull;
use Symfony\Component\Validator\Constraints as Assert;

/**
 * Pulling the changes of synced records after a cursor: `{"changes": [...], "cursor": "...", "hasMore": bool}`. Start
 * without a cursor (everything live), store the cursor, and call again with it while hasMore is true. 410 means the
 * cursor is too old: pull everything again without one. The wire format is in docs/sync.md.
 */
#[ApiResource(
    shortName: 'SyncChanges',
    routePrefix: '/api/v1',
    operations: [
        new Get(
            uriTemplate: '/sync/organizations/{organizationId}/changes',
            uriVariables: [],
            security: "is_granted('".Scope::ROLE_SYNC."')",
            provider: SyncChangesProvider::class,
            parameters: [
                'cursor' => new QueryParameter(
                    schema: ['type' => 'string'],
                    description: 'The cursor from the previous response; leave it out for a first pull.',
                    constraints: [new Assert\Length(max: 200)],
                ),
                'limit' => new QueryParameter(
                    schema: ['type' => 'integer', 'minimum' => 1, 'maximum' => RecordPull::MAX_LIMIT, 'default' => RecordPull::DEFAULT_LIMIT],
                    description: 'Records per page at most; a page also holds no more than fit in 4 MiB of payload.',
                    constraints: [new Assert\Regex('/^\d{1,4}$/', message: 'limit must be a whole number.'), new Assert\Range(min: 1, max: RecordPull::MAX_LIMIT)],
                ),
            ],
            name: 'sync_changes_organization',
        ),
        new Get(
            uriTemplate: '/sync/user/changes',
            uriVariables: [],
            security: "is_granted('".Scope::ROLE_SYNC."')",
            provider: SyncChangesProvider::class,
            parameters: [
                'cursor' => new QueryParameter(
                    schema: ['type' => 'string'],
                    description: 'The cursor from the previous response; leave it out for a first pull.',
                    constraints: [new Assert\Length(max: 200)],
                ),
                'limit' => new QueryParameter(
                    schema: ['type' => 'integer', 'minimum' => 1, 'maximum' => RecordPull::MAX_LIMIT, 'default' => RecordPull::DEFAULT_LIMIT],
                    description: 'Records per page at most; a page also holds no more than fit in 4 MiB of payload.',
                    constraints: [new Assert\Regex('/^\d{1,4}$/', message: 'limit must be a whole number.'), new Assert\Range(min: 1, max: RecordPull::MAX_LIMIT)],
                ),
            ],
            name: 'sync_changes_user',
        ),
    ],
)]
final class SyncChanges
{
}
