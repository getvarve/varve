<?php

declare(strict_types=1);

namespace DoctrineMigrations;

use Doctrine\DBAL\Schema\Schema;
use Doctrine\Migrations\AbstractMigration;

/**
 * A confirmation link carries the name and the password hash of the sign-up it was sent for, and an address is at
 * most as long as the OAuth2 server's token tables hold (128 characters).
 *
 * A confirmation link made before this has no sign-up with it and no longer confirms anything: its account signs up
 * again. An address longer than 128 characters could never sign in an app; none may exist when this runs.
 */
final class Version20261112134529 extends AbstractMigration
{
    public function getDescription(): string
    {
        return 'Pending name and password on confirmation links, addresses of at most 128 characters';
    }

    public function up(Schema $schema): void
    {
        $this->addSql('ALTER TABLE account_link ADD pending_display_name VARCHAR(120) DEFAULT NULL');
        $this->addSql('ALTER TABLE account_link ADD pending_password VARCHAR(255) DEFAULT NULL');
        $this->addSql('ALTER TABLE app_user ALTER email TYPE VARCHAR(128)');
    }

    public function down(Schema $schema): void
    {
        $this->addSql('ALTER TABLE account_link DROP pending_display_name');
        $this->addSql('ALTER TABLE account_link DROP pending_password');
        $this->addSql('ALTER TABLE app_user ALTER email TYPE VARCHAR(320)');
    }
}
