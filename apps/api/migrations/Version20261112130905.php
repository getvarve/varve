<?php

declare(strict_types=1);

namespace DoctrineMigrations;

use Doctrine\DBAL\Schema\Schema;
use Doctrine\Migrations\AbstractMigration;

/**
 * Personal sync: an organization's records (connection profiles, folders, saved queries), a user's own records
 * (favourites, preferences, overrides), revisions of saved queries, and the change numbers of both feeds.
 */
final class Version20261112130905 extends AbstractMigration
{
    public function getDescription(): string
    {
        return 'Synced records, per-user records, record revisions and their change numbers';
    }

    public function up(Schema $schema): void
    {
        $this->addSql('CREATE TABLE record (id UUID NOT NULL, kind VARCHAR(16) NOT NULL, payload JSON DEFAULT NULL, version INT NOT NULL, change_seq BIGINT NOT NULL, created_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, updated_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, deleted_at TIMESTAMP(0) WITHOUT TIME ZONE DEFAULT NULL, organization_id UUID NOT NULL, updated_by_id UUID DEFAULT NULL, updated_device_id UUID DEFAULT NULL, PRIMARY KEY (organization_id, id))');
        // A tombstone keeps no payload, and a live record always has one.
        $this->addSql('ALTER TABLE record ADD CONSTRAINT record_tombstone_without_payload CHECK ((deleted_at IS NULL) = (payload IS NOT NULL))');
        $this->addSql('CREATE INDEX IDX_9B349F914AF38FD1 ON record (deleted_at)');
        $this->addSql('CREATE UNIQUE INDEX UNIQ_9B349F9132C8A3DE475BB34E ON record (organization_id, change_seq)');
        $this->addSql('CREATE INDEX IDX_9B349F9132C8A3DE ON record (organization_id)');
        $this->addSql('CREATE INDEX IDX_9B349F91896DBBDE ON record (updated_by_id)');
        $this->addSql('CREATE INDEX IDX_9B349F914F00C88B ON record (updated_device_id)');
        $this->addSql('CREATE TABLE record_revision (id UUID NOT NULL, version INT NOT NULL, payload JSON NOT NULL, updated_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, organization_id UUID NOT NULL, record_id UUID NOT NULL, updated_by_id UUID DEFAULT NULL, updated_device_id UUID DEFAULT NULL, PRIMARY KEY (id))');
        $this->addSql('CREATE UNIQUE INDEX UNIQ_4360C74D32C8A3DE4DFD750CBF1CD3C3 ON record_revision (organization_id, record_id, version)');
        $this->addSql('CREATE INDEX IDX_4360C74D32C8A3DE4DFD750C ON record_revision (organization_id, record_id)');
        $this->addSql('CREATE INDEX IDX_4360C74D896DBBDE ON record_revision (updated_by_id)');
        $this->addSql('CREATE INDEX IDX_4360C74D4F00C88B ON record_revision (updated_device_id)');
        $this->addSql('CREATE TABLE user_record (id UUID NOT NULL, kind VARCHAR(16) NOT NULL, payload JSON DEFAULT NULL, version INT NOT NULL, change_seq BIGINT NOT NULL, created_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, updated_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, deleted_at TIMESTAMP(0) WITHOUT TIME ZONE DEFAULT NULL, user_id UUID NOT NULL, updated_device_id UUID DEFAULT NULL, PRIMARY KEY (user_id, id))');
        $this->addSql('ALTER TABLE user_record ADD CONSTRAINT user_record_tombstone_without_payload CHECK ((deleted_at IS NULL) = (payload IS NOT NULL))');
        $this->addSql('CREATE INDEX IDX_FE6684AC4AF38FD1 ON user_record (deleted_at)');
        $this->addSql('CREATE UNIQUE INDEX UNIQ_FE6684ACA76ED395475BB34E ON user_record (user_id, change_seq)');
        $this->addSql('CREATE INDEX IDX_FE6684ACA76ED395 ON user_record (user_id)');
        $this->addSql('CREATE INDEX IDX_FE6684AC4F00C88B ON user_record (updated_device_id)');
        $this->addSql('ALTER TABLE record ADD CONSTRAINT FK_9B349F9132C8A3DE FOREIGN KEY (organization_id) REFERENCES organization (id) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE record ADD CONSTRAINT FK_9B349F91896DBBDE FOREIGN KEY (updated_by_id) REFERENCES app_user (id) ON DELETE SET NULL NOT DEFERRABLE');
        $this->addSql('ALTER TABLE record ADD CONSTRAINT FK_9B349F914F00C88B FOREIGN KEY (updated_device_id) REFERENCES device (id) ON DELETE SET NULL NOT DEFERRABLE');
        $this->addSql('ALTER TABLE record_revision ADD CONSTRAINT FK_4360C74D32C8A3DE4DFD750C FOREIGN KEY (organization_id, record_id) REFERENCES record (organization_id, id) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE record_revision ADD CONSTRAINT FK_4360C74D896DBBDE FOREIGN KEY (updated_by_id) REFERENCES app_user (id) ON DELETE SET NULL NOT DEFERRABLE');
        $this->addSql('ALTER TABLE record_revision ADD CONSTRAINT FK_4360C74D4F00C88B FOREIGN KEY (updated_device_id) REFERENCES device (id) ON DELETE SET NULL NOT DEFERRABLE');
        $this->addSql('ALTER TABLE user_record ADD CONSTRAINT FK_FE6684ACA76ED395 FOREIGN KEY (user_id) REFERENCES app_user (id) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE user_record ADD CONSTRAINT FK_FE6684AC4F00C88B FOREIGN KEY (updated_device_id) REFERENCES device (id) ON DELETE SET NULL NOT DEFERRABLE');
        $this->addSql('ALTER TABLE app_user ADD record_seq BIGINT DEFAULT 0 NOT NULL');
        $this->addSql('ALTER TABLE app_user ADD record_purge_seq BIGINT DEFAULT 0 NOT NULL');
        $this->addSql('ALTER TABLE organization ADD record_seq BIGINT DEFAULT 0 NOT NULL');
        $this->addSql('ALTER TABLE organization ADD record_purge_seq BIGINT DEFAULT 0 NOT NULL');
    }

    public function down(Schema $schema): void
    {
        $this->addSql('ALTER TABLE record DROP CONSTRAINT FK_9B349F9132C8A3DE');
        $this->addSql('ALTER TABLE record DROP CONSTRAINT FK_9B349F91896DBBDE');
        $this->addSql('ALTER TABLE record DROP CONSTRAINT FK_9B349F914F00C88B');
        $this->addSql('ALTER TABLE record_revision DROP CONSTRAINT FK_4360C74D32C8A3DE4DFD750C');
        $this->addSql('ALTER TABLE record_revision DROP CONSTRAINT FK_4360C74D896DBBDE');
        $this->addSql('ALTER TABLE record_revision DROP CONSTRAINT FK_4360C74D4F00C88B');
        $this->addSql('ALTER TABLE user_record DROP CONSTRAINT FK_FE6684ACA76ED395');
        $this->addSql('ALTER TABLE user_record DROP CONSTRAINT FK_FE6684AC4F00C88B');
        $this->addSql('DROP TABLE record');
        $this->addSql('DROP TABLE record_revision');
        $this->addSql('DROP TABLE user_record');
        $this->addSql('ALTER TABLE app_user DROP record_seq');
        $this->addSql('ALTER TABLE app_user DROP record_purge_seq');
        $this->addSql('ALTER TABLE organization DROP record_seq');
        $this->addSql('ALTER TABLE organization DROP record_purge_seq');
    }
}
