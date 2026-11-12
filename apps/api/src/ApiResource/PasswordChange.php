<?php

declare(strict_types=1);

namespace App\ApiResource;

use ApiPlatform\Metadata\ApiResource;
use ApiPlatform\Metadata\Post;
use App\ApiResource\Input\ChangePassword;
use App\Security\FirstPartyClients;
use App\Security\Scope;
use App\State\PasswordChangeProcessor;

/**
 * Changing the signed-in user's password, with the current one. The device that asks stays signed in; every other
 * device is signed out (its tokens stop working at once), and so is the browser's "keep me signed in". 204 when done;
 * 422 on a wrong current password or a new one that is too short; 429 after 5 checks in 15 minutes.
 */
#[ApiResource(
    shortName: 'PasswordChange',
    routePrefix: '/api/v1',
    operations: [
        new Post(
            uriTemplate: '/me/password',
            status: 204,
            security: "is_granted('".Scope::ROLE_ACCOUNT."') and is_granted('".FirstPartyClients::ATTRIBUTE."')",
            input: ChangePassword::class,
            output: false,
            read: false,
            processor: PasswordChangeProcessor::class,
        ),
    ],
)]
final class PasswordChange
{
}
