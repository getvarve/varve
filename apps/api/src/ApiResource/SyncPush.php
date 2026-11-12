<?php

declare(strict_types=1);

namespace App\ApiResource;

use ApiPlatform\Metadata\ApiResource;
use ApiPlatform\Metadata\Post;
use App\Security\Scope;
use App\State\SyncPushProcessor;

/**
 * Pushing changes to synced records: `{"changes": [...]}`, at most 100 upserts or deletes, each based on the version
 * of the record the client last saw. The answer has one result per change, in the same order: accepted with the new
 * version, a conflict with the server's copy, or rejected with a reason (and the limit, when one is the reason). The
 * wire format is in docs/sync.md.
 *
 * The body is read as JSON by the processor itself rather than deserialized, so payloads come back exactly as they
 * were pushed (an empty object stays an object).
 */
#[ApiResource(
    shortName: 'SyncPush',
    routePrefix: '/api/v1',
    operations: [
        new Post(
            uriTemplate: '/sync/organizations/{organizationId}/push',
            status: 200,
            security: "is_granted('".Scope::ROLE_SYNC."')",
            read: false,
            deserialize: false,
            validate: false,
            processor: SyncPushProcessor::class,
            name: 'sync_push_organization',
        ),
        new Post(
            uriTemplate: '/sync/user/push',
            status: 200,
            security: "is_granted('".Scope::ROLE_SYNC."')",
            read: false,
            deserialize: false,
            validate: false,
            processor: SyncPushProcessor::class,
            name: 'sync_push_user',
        ),
    ],
)]
final class SyncPush
{
}
