<?php

declare(strict_types=1);

namespace App\ApiResource;

use ApiPlatform\Metadata\ApiResource;
use ApiPlatform\Metadata\Post;
use App\ApiResource\Input\DeleteAccount;
use App\Security\FirstPartyClients;
use App\Security\Scope;
use App\State\AccountDeletionProcessor;

/**
 * Deleting the signed-in user's account, with the current password: the user, their personal organization and their
 * devices go at once, and every token stops working. 204 when done; 422 on a wrong password; 429 after 5 checks in
 * 15 minutes.
 */
#[ApiResource(
    shortName: 'AccountDeletion',
    routePrefix: '/api/v1',
    operations: [
        new Post(
            uriTemplate: '/me/deletion',
            status: 204,
            security: "is_granted('".Scope::ROLE_ACCOUNT."') and is_granted('".FirstPartyClients::ATTRIBUTE."')",
            input: DeleteAccount::class,
            output: false,
            read: false,
            processor: AccountDeletionProcessor::class,
        ),
    ],
)]
final class AccountDeletion
{
}
