<?php

declare(strict_types=1);

namespace App\Command;

use App\Account\UserDeletion;
use Doctrine\DBAL\Connection;
use Symfony\Component\Console\Attribute\Argument;
use Symfony\Component\Console\Attribute\AsCommand;
use Symfony\Component\Console\Attribute\Option;
use Symfony\Component\Console\Command\Command;
use Symfony\Component\Console\Style\SymfonyStyle;
use Symfony\Component\Uid\Uuid;

/**
 * Deletes a user and everything they own ({@see UserDeletion}), for an operator. Users delete their own account in the
 * app (POST /api/v1/me/deletion).
 */
#[AsCommand(name: 'app:users:delete', description: 'Delete a user and everything they own')]
final readonly class DeleteUserCommand
{
    public function __construct(
        private Connection $connection,
        private UserDeletion $deletion,
    ) {
    }

    public function __invoke(
        SymfonyStyle $io,
        #[Argument('Email address of the user to delete')]
        string $email,
        #[Option('Delete without asking')]
        bool $force = false,
    ): int {
        $id = $this->connection->fetchOne('SELECT id FROM app_user WHERE email = :email', ['email' => mb_strtolower(trim($email))]);
        if (!\is_string($id)) {
            $io->error(\sprintf('No user has the email address "%s".', $email));

            return Command::FAILURE;
        }
        if (!$force && !$io->confirm(\sprintf('Delete %s and everything they own? This cannot be undone; database backups keep the data until they expire.', $email), false)) {
            $io->writeln('Nothing deleted.');

            return Command::FAILURE;
        }

        $this->deletion->delete(Uuid::fromString($id));
        $io->success(\sprintf('Deleted %s and everything they owned.', $email));

        return Command::SUCCESS;
    }
}
