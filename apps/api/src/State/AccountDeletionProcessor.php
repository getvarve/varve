<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProcessorInterface;
use App\Account\PasswordCheck;
use App\Account\TooManyAttempts;
use App\Account\UserDeletion;
use App\ApiResource\Input\DeleteAccount;
use App\Security\CurrentUser;
use Symfony\Component\HttpKernel\Exception\TooManyRequestsHttpException;

/**
 * @implements ProcessorInterface<DeleteAccount, null>
 */
final readonly class AccountDeletionProcessor implements ProcessorInterface
{
    public function __construct(
        private CurrentUser $currentUser,
        private PasswordCheck $check,
        private UserDeletion $deletion,
    ) {
    }

    public function process(mixed $data, Operation $operation, array $uriVariables = [], array $context = []): null
    {
        $user = $this->currentUser->get();
        try {
            $current = $this->check->isCurrent($user, $data->password);
        } catch (TooManyAttempts $exception) {
            throw new TooManyRequestsHttpException(max(1, $exception->retryAfter->getTimestamp() - time()), 'Too many password checks. Try again later.');
        }
        if (!$current) {
            throw Violation::on('password', 'This is not your current password.');
        }

        $this->deletion->delete($user->getId());

        return null;
    }
}
