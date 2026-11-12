<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProcessorInterface;
use App\Account\AccountLinks;
use App\Account\PasswordCheck;
use App\Account\PasswordPolicy;
use App\Account\TooManyAttempts;
use App\ApiResource\Input\ChangePassword;
use App\Device\CurrentDevice;
use App\Device\Devices;
use App\Enum\AccountLinkPurpose;
use App\Security\CurrentUser;
use Doctrine\ORM\EntityManagerInterface;
use Symfony\Component\HttpKernel\Exception\TooManyRequestsHttpException;
use Symfony\Component\PasswordHasher\Hasher\UserPasswordHasherInterface;

/**
 * @implements ProcessorInterface<ChangePassword, null>
 */
final readonly class PasswordChangeProcessor implements ProcessorInterface
{
    public function __construct(
        private CurrentUser $currentUser,
        private CurrentDevice $currentDevice,
        private PasswordCheck $check,
        private UserPasswordHasherInterface $passwordHasher,
        private Devices $devices,
        private AccountLinks $links,
        private EntityManagerInterface $entityManager,
    ) {
    }

    public function process(mixed $data, Operation $operation, array $uriVariables = [], array $context = []): null
    {
        $user = $this->currentUser->get();
        $problem = PasswordPolicy::problem($data->newPassword);
        if (null !== $problem) {
            throw Violation::on('newPassword', $problem);
        }
        try {
            $current = $this->check->isCurrent($user, $data->currentPassword);
        } catch (TooManyAttempts $exception) {
            throw new TooManyRequestsHttpException(max(1, $exception->retryAfter->getTimestamp() - time()), 'Too many password checks. Try again later.');
        }
        if (!$current) {
            throw Violation::on('currentPassword', 'This is not your current password.');
        }

        $keep = $this->currentDevice->id();
        $this->entityManager->wrapInTransaction(function () use ($user, $data, $keep): void {
            $user->setPassword($this->passwordHasher->hashPassword($user, $data->newPassword));
            $this->entityManager->flush();
            // A reset link asked for before this change must not set yet another password afterwards.
            $this->links->end($user, AccountLinkPurpose::ResetPassword);
            $this->devices->signOutOthers($user, $keep);
        });

        return null;
    }
}
