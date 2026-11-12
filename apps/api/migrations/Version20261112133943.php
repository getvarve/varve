<?php

declare(strict_types=1);

namespace DoctrineMigrations;

use Doctrine\DBAL\Schema\Schema;
use Doctrine\Migrations\AbstractMigration;

/**
 * Sync storage: the size of every stored payload beside it (for the byte limits of a feed, the byte limit of a saved
 * query's revisions and the size of a page of a pull), and who created an organization's record.
 */
final class Version20261112133943 extends AbstractMigration
{
    public function getDescription(): string
    {
        return 'Payload sizes of synced records and revisions, and the creator of a record';
    }

    public function up(Schema $schema): void
    {
        $this->addSql('ALTER TABLE record ADD payload_bytes INT DEFAULT 0 NOT NULL');
        $this->addSql('ALTER TABLE record ADD created_by_id UUID DEFAULT NULL');
        $this->addSql('ALTER TABLE record ADD CONSTRAINT FK_9B349F91B03A8386 FOREIGN KEY (created_by_id) REFERENCES app_user (id) ON DELETE SET NULL NOT DEFERRABLE');
        $this->addSql('CREATE INDEX IDX_9B349F9132C8A3DE22C21ABE ON record (organization_id, payload_bytes) WHERE (deleted_at IS NULL)');
        $this->addSql('CREATE INDEX IDX_9B349F91B03A8386 ON record (created_by_id)');
        $this->addSql('ALTER TABLE record_revision ADD payload_bytes INT DEFAULT 0 NOT NULL');
        $this->addSql('ALTER TABLE user_record ADD payload_bytes INT DEFAULT 0 NOT NULL');
        $this->addSql('CREATE INDEX IDX_FE6684ACA76ED39522C21ABE ON user_record (user_id, payload_bytes) WHERE (deleted_at IS NULL)');
        // Rows from before this migration: the size is the stored JSON's, as a push counts it; tombstones stay at 0.
        $this->addSql('UPDATE record SET payload_bytes = octet_length(payload::text) WHERE payload IS NOT NULL');
        $this->addSql('UPDATE user_record SET payload_bytes = octet_length(payload::text) WHERE payload IS NOT NULL');
        $this->addSql('UPDATE record_revision SET payload_bytes = octet_length(payload::text)');
        // Until now every organization was personal, so its owner created its records.
        $this->addSql('UPDATE record SET created_by_id = o.personal_owner_id FROM organization o WHERE o.id = record.organization_id');
    }

    public function down(Schema $schema): void
    {
        $this->addSql('ALTER TABLE record DROP CONSTRAINT FK_9B349F91B03A8386');
        $this->addSql('DROP INDEX IDX_9B349F9132C8A3DE22C21ABE');
        $this->addSql('DROP INDEX IDX_9B349F91B03A8386');
        $this->addSql('ALTER TABLE record DROP payload_bytes');
        $this->addSql('ALTER TABLE record DROP created_by_id');
        $this->addSql('ALTER TABLE record_revision DROP payload_bytes');
        $this->addSql('DROP INDEX IDX_FE6684ACA76ED39522C21ABE');
        $this->addSql('ALTER TABLE user_record DROP payload_bytes');
    }
}
