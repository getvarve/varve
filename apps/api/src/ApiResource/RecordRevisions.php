<?php

declare(strict_types=1);

namespace App\ApiResource;

use ApiPlatform\Metadata\ApiResource;
use ApiPlatform\Metadata\Get;
use App\Security\Scope;
use App\State\RecordRevisionsProvider;

/**
 * The earlier versions of a saved query (design 8.11), newest first: `{"revisions": [...]}`, the last 50. A record of
 * another kind has none; a record the organization does not have is a 404. The wire format is in docs/sync.md.
 */
#[ApiResource(
    shortName: 'RecordRevisions',
    routePrefix: '/api/v1',
    operations: [
        new Get(
            uriTemplate: '/sync/organizations/{organizationId}/records/{recordId}/revisions',
            uriVariables: [],
            security: "is_granted('".Scope::ROLE_SYNC."')",
            provider: RecordRevisionsProvider::class,
            name: 'sync_record_revisions',
        ),
    ],
)]
final class RecordRevisions
{
}
