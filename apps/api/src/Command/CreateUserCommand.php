<?php

declare(strict_types=1);

namespace App\Command;

use App\Account\AccountDetails;
use App\Account\PasswordPolicy;
use App\Repository\UserRepository;
use App\Service\UserProvisioner;
use Symfony\Component\Console\Attribute\Argument;
use Symfony\Component\Console\Attribute\AsCommand;
use Symfony\Component\Console\Command\Command;
use Symfony\Component\Console\Style\SymfonyStyle;

/**
 * Creates a user with their personal organization, for an operator: the way to add users to a server whose sign-up
 * is closed (VARVE_SIGN_UP=0). The address counts as confirmed, so the user can sign in at once; no mail is sent.
 * The operator types the password and passes it on; the user can change it in the app.
 */
#[AsCommand(name: 'app:users:create', description: 'Create a user who can sign in at once')]
final readonly class CreateUserCommand
{
    public function __construct(
        private UserProvisioner $provisioner,
        private UserRepository $users,
        private AccountDetails $details,
    ) {
    }

    public function __invoke(
        SymfonyStyle $io,
        #[Argument('Email address the user signs in with')]
        string $email,
        #[Argument('Name of the user')]
        string $name,
    ): int {
        $email = trim($email);
        $name = trim($name);
        $problem = $this->details->emailProblem($email) ?? AccountDetails::nameProblem($name);
        if (null !== $problem) {
            $io->error($problem);

            return Command::FAILURE;
        }
        if (null !== $this->users->findOneByEmail($email)) {
            $io->error(\sprintf('A user with the email address "%s" already exists.', $email));

            return Command::FAILURE;
        }

        $password = $io->askHidden(\sprintf('Password (at least %d characters)', PasswordPolicy::MIN_LENGTH), static function (mixed $value): string {
            $problem = PasswordPolicy::problem(\is_string($value) ? $value : '');
            if (null !== $problem) {
                throw new \RuntimeException($problem);
            }
            \assert(\is_string($value));

            return $value;
        });
        \assert(\is_string($password));

        $user = $this->provisioner->provision($email, $name, $password, true);
        $io->success(\sprintf('Created user %s (%s).', $user->getEmail(), $user->getId()->toRfc4122()));

        return Command::SUCCESS;
    }
}
