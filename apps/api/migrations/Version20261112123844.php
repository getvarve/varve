<?php

declare(strict_types=1);

namespace DoctrineMigrations;

use Doctrine\DBAL\Schema\Schema;
use Doctrine\Migrations\AbstractMigration;

/**
 * Accounts and sign-in: users with their personal organizations, the OAuth2 server's clients and tokens, the devices
 * those tokens belong to, and the links of account mail.
 */
final class Version20261112123844 extends AbstractMigration
{
    public function getDescription(): string
    {
        return 'Users, personal organizations, OAuth2 clients and tokens, devices and account links';
    }

    public function up(Schema $schema): void
    {
        $this->addSql('CREATE TABLE account_link (id UUID NOT NULL, purpose VARCHAR(16) NOT NULL, token_hash CHAR(64) NOT NULL, created_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, expires_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, user_id UUID NOT NULL, PRIMARY KEY (id))');
        $this->addSql('CREATE INDEX IDX_BA3F1E5BA76ED395B887B3EB ON account_link (user_id, purpose)');
        $this->addSql('CREATE INDEX IDX_BA3F1E5BF9D83E2 ON account_link (expires_at)');
        $this->addSql('CREATE UNIQUE INDEX UNIQ_BA3F1E5BB3BC57DA ON account_link (token_hash)');
        $this->addSql('CREATE INDEX IDX_BA3F1E5BA76ED395 ON account_link (user_id)');
        $this->addSql('CREATE TABLE app_user (id UUID NOT NULL, email VARCHAR(320) NOT NULL, password VARCHAR(255) NOT NULL, display_name VARCHAR(120) NOT NULL, roles JSON NOT NULL, email_confirmed_at TIMESTAMP(0) WITHOUT TIME ZONE DEFAULT NULL, created_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, PRIMARY KEY (id))');
        $this->addSql('CREATE UNIQUE INDEX UNIQ_88BDF3E9E7927C74 ON app_user (email)');
        $this->addSql('CREATE INDEX IDX_88BDF3E9FBB048E68B8E8428 ON app_user (email_confirmed_at, created_at)');
        $this->addSql('CREATE TABLE device (id UUID NOT NULL, name VARCHAR(100) NOT NULL, platform VARCHAR(16) NOT NULL, app_version VARCHAR(32) NOT NULL, created_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, last_seen_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, user_id UUID NOT NULL, client VARCHAR(32) NOT NULL, PRIMARY KEY (id))');
        $this->addSql('CREATE INDEX IDX_92FB68EA76ED395 ON device (user_id)');
        $this->addSql('CREATE INDEX IDX_92FB68EC7440455 ON device (client)');
        $this->addSql('CREATE TABLE device_session (created_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, access_token CHAR(80) NOT NULL, device_id UUID NOT NULL, PRIMARY KEY (access_token))');
        $this->addSql('CREATE INDEX IDX_5AF20C4994A4C7D4 ON device_session (device_id)');
        $this->addSql('CREATE TABLE oauth2_access_token (identifier CHAR(80) NOT NULL, expiry TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, user_identifier VARCHAR(128) DEFAULT NULL, scopes TEXT DEFAULT NULL, revoked BOOLEAN NOT NULL, client VARCHAR(32) NOT NULL, PRIMARY KEY (identifier))');
        $this->addSql('CREATE INDEX IDX_454D9673C7440455 ON oauth2_access_token (client)');
        $this->addSql('CREATE TABLE oauth2_authorization_code (identifier CHAR(80) NOT NULL, expiry TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, user_identifier VARCHAR(128) DEFAULT NULL, scopes TEXT DEFAULT NULL, revoked BOOLEAN NOT NULL, client VARCHAR(32) NOT NULL, PRIMARY KEY (identifier))');
        $this->addSql('CREATE INDEX IDX_509FEF5FC7440455 ON oauth2_authorization_code (client)');
        $this->addSql('CREATE TABLE oauth2_client (name VARCHAR(128) NOT NULL, secret VARCHAR(128) DEFAULT NULL, redirect_uris TEXT DEFAULT NULL, grants TEXT DEFAULT NULL, scopes TEXT DEFAULT NULL, active BOOLEAN NOT NULL, allow_plain_text_pkce BOOLEAN DEFAULT false NOT NULL, identifier VARCHAR(32) NOT NULL, PRIMARY KEY (identifier))');
        $this->addSql('CREATE TABLE oauth2_refresh_token (identifier CHAR(80) NOT NULL, expiry TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, revoked BOOLEAN NOT NULL, access_token CHAR(80) DEFAULT NULL, PRIMARY KEY (identifier))');
        $this->addSql('CREATE INDEX IDX_4DD90732B6A2DD68 ON oauth2_refresh_token (access_token)');
        $this->addSql('CREATE TABLE organization (id UUID NOT NULL, name VARCHAR(120) DEFAULT NULL, created_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, personal_owner_id UUID DEFAULT NULL, PRIMARY KEY (id))');
        $this->addSql('CREATE UNIQUE INDEX UNIQ_C1EE637CD203BFB0 ON organization (personal_owner_id)');
        // A team has a name and no personal owner; a personal organization goes by its owner's name.
        $this->addSql('ALTER TABLE organization ADD CONSTRAINT organization_team_or_personal CHECK ((personal_owner_id IS NULL) <> (name IS NULL))');
        $this->addSql('CREATE TABLE organization_member (id UUID NOT NULL, role VARCHAR(16) NOT NULL, joined_at TIMESTAMP(0) WITHOUT TIME ZONE NOT NULL, organization_id UUID NOT NULL, user_id UUID NOT NULL, PRIMARY KEY (id))');
        $this->addSql('CREATE INDEX IDX_756A2A8DA76ED395 ON organization_member (user_id)');
        $this->addSql('CREATE UNIQUE INDEX UNIQ_756A2A8D32C8A3DEA76ED395 ON organization_member (organization_id, user_id)');
        $this->addSql('CREATE INDEX IDX_756A2A8D32C8A3DE ON organization_member (organization_id)');
        $this->addSql('ALTER TABLE account_link ADD CONSTRAINT FK_BA3F1E5BA76ED395 FOREIGN KEY (user_id) REFERENCES app_user (id) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE device ADD CONSTRAINT FK_92FB68EA76ED395 FOREIGN KEY (user_id) REFERENCES app_user (id) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE device ADD CONSTRAINT FK_92FB68EC7440455 FOREIGN KEY (client) REFERENCES oauth2_client (identifier) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE device_session ADD CONSTRAINT FK_5AF20C49B6A2DD68 FOREIGN KEY (access_token) REFERENCES oauth2_access_token (identifier) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE device_session ADD CONSTRAINT FK_5AF20C4994A4C7D4 FOREIGN KEY (device_id) REFERENCES device (id) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE oauth2_access_token ADD CONSTRAINT FK_454D9673C7440455 FOREIGN KEY (client) REFERENCES oauth2_client (identifier) ON DELETE CASCADE');
        $this->addSql('ALTER TABLE oauth2_authorization_code ADD CONSTRAINT FK_509FEF5FC7440455 FOREIGN KEY (client) REFERENCES oauth2_client (identifier) ON DELETE CASCADE');
        $this->addSql('ALTER TABLE oauth2_refresh_token ADD CONSTRAINT FK_4DD90732B6A2DD68 FOREIGN KEY (access_token) REFERENCES oauth2_access_token (identifier) ON DELETE SET NULL');
        $this->addSql('ALTER TABLE organization ADD CONSTRAINT FK_C1EE637CD203BFB0 FOREIGN KEY (personal_owner_id) REFERENCES app_user (id) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE organization_member ADD CONSTRAINT FK_756A2A8D32C8A3DE FOREIGN KEY (organization_id) REFERENCES organization (id) ON DELETE CASCADE NOT DEFERRABLE');
        $this->addSql('ALTER TABLE organization_member ADD CONSTRAINT FK_756A2A8DA76ED395 FOREIGN KEY (user_id) REFERENCES app_user (id) ON DELETE CASCADE NOT DEFERRABLE');
    }

    public function down(Schema $schema): void
    {
        $this->addSql('ALTER TABLE account_link DROP CONSTRAINT FK_BA3F1E5BA76ED395');
        $this->addSql('ALTER TABLE device DROP CONSTRAINT FK_92FB68EA76ED395');
        $this->addSql('ALTER TABLE device DROP CONSTRAINT FK_92FB68EC7440455');
        $this->addSql('ALTER TABLE device_session DROP CONSTRAINT FK_5AF20C49B6A2DD68');
        $this->addSql('ALTER TABLE device_session DROP CONSTRAINT FK_5AF20C4994A4C7D4');
        $this->addSql('ALTER TABLE oauth2_access_token DROP CONSTRAINT FK_454D9673C7440455');
        $this->addSql('ALTER TABLE oauth2_authorization_code DROP CONSTRAINT FK_509FEF5FC7440455');
        $this->addSql('ALTER TABLE oauth2_refresh_token DROP CONSTRAINT FK_4DD90732B6A2DD68');
        $this->addSql('ALTER TABLE organization DROP CONSTRAINT FK_C1EE637CD203BFB0');
        $this->addSql('ALTER TABLE organization_member DROP CONSTRAINT FK_756A2A8D32C8A3DE');
        $this->addSql('ALTER TABLE organization_member DROP CONSTRAINT FK_756A2A8DA76ED395');
        $this->addSql('DROP TABLE account_link');
        $this->addSql('DROP TABLE app_user');
        $this->addSql('DROP TABLE device');
        $this->addSql('DROP TABLE device_session');
        $this->addSql('DROP TABLE oauth2_access_token');
        $this->addSql('DROP TABLE oauth2_authorization_code');
        $this->addSql('DROP TABLE oauth2_client');
        $this->addSql('DROP TABLE oauth2_refresh_token');
        $this->addSql('DROP TABLE organization');
        $this->addSql('DROP TABLE organization_member');
    }
}
