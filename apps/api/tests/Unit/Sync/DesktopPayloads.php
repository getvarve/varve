<?php

declare(strict_types=1);

namespace App\Tests\Unit\Sync;

/**
 * Whole payloads as the desktop app serializes its records (crates/records/src/model.rs: Folder, ConnectionProfile,
 * SavedQuery, with every field, in camelCase), and per-user records as a client would write them. None holds a
 * secret, and the server must take every one of them: a rule of the secret guard that refuses one of these is wrong.
 */
final class DesktopPayloads
{
    private const string FOLDER = '0196f3c2-1111-7000-8000-000000000001';
    private const string CONNECTION = '0196f3c2-2222-7000-8000-000000000002';

    /**
     * @return array<string, array{string, array<string, mixed>}> a kind and a payload under a name, for the record
     *                                                            with this id
     */
    public static function all(string $id): array
    {
        $stamps = ['createdAt' => 1762945200000, 'updatedAt' => 1762948800000, 'version' => 3];

        return [
            'a folder' => ['folder', ['id' => $id, 'workspaceId' => 'personal', 'parentId' => null, 'name' => 'Webshop', 'color' => null, 'sortOrder' => 0, ...$stamps]],
            'a folder in a folder, named after secrets' => ['folder', ['id' => $id, 'workspaceId' => 'personal', 'parentId' => self::FOLDER, 'name' => 'Passwords, tokens & API keys', 'color' => 'purple', 'sortOrder' => 4, ...$stamps]],
            'a PostgreSQL connection through an SSH tunnel with a key file' => ['connection', [
                'id' => $id,
                'workspaceId' => 'personal',
                'folderId' => self::FOLDER,
                'name' => 'Shop production',
                'color' => 'red',
                'sortOrder' => 1,
                'engine' => 'postgres',
                'host' => 'db.shop.example.test',
                'port' => 5432,
                'database' => 'shop',
                'username' => 'kim',
                'environment' => 'production',
                'tls' => ['mode' => 'verify_full', 'caPath' => '/etc/ssl/certs/shop-ca.pem', 'clientCertPath' => '/Users/kim/.varve/client.crt', 'clientKeyPath' => '/Users/kim/.varve/client.key'],
                'ssh' => ['enabled' => true, 'host' => 'bastion.shop.example.test', 'port' => 2222, 'username' => 'kim', 'auth' => 'key_file', 'keyPath' => '/Users/kim/.ssh/id_ed25519'],
                'options' => ['readOnly' => true, 'confirmDestructive' => true, 'statementTimeoutMs' => 30000, 'keepaliveS' => 60, 'initSql' => "SET search_path TO shop, public;\nSET application_name = 'varve';", 'showSystemObjects' => false, 'redisDb' => 0],
                'savedSecrets' => ['db_password', 'ssh_password', 'ssh_key_passphrase', 'tls_key_passphrase'],
                ...$stamps,
            ]],
            'a MySQL connection with every default' => ['connection', [
                'id' => $id,
                'workspaceId' => 'personal',
                'folderId' => null,
                'name' => 'Local MySQL',
                'color' => null,
                'sortOrder' => 0,
                'engine' => 'mysql',
                'host' => '127.0.0.1',
                'port' => 3306,
                'database' => null,
                'username' => null,
                'environment' => 'development',
                'tls' => ['mode' => 'prefer', 'caPath' => null, 'clientCertPath' => null, 'clientKeyPath' => null],
                'ssh' => ['enabled' => false, 'host' => '', 'port' => 22, 'username' => '', 'auth' => 'password', 'keyPath' => null],
                'options' => ['readOnly' => false, 'confirmDestructive' => true, 'statementTimeoutMs' => null, 'keepaliveS' => null, 'initSql' => null, 'showSystemObjects' => false, 'redisDb' => 0],
                'savedSecrets' => [],
                ...$stamps,
            ]],
            'a MariaDB connection over an IPv6 address with the SSH agent' => ['connection', [
                'id' => $id,
                'workspaceId' => 'personal',
                'folderId' => self::FOLDER,
                'name' => 'Staging (MariaDB)',
                'color' => 'orange',
                'sortOrder' => 2,
                'engine' => 'mariadb',
                'host' => '2001:db8::10',
                'port' => 3307,
                'database' => 'shop_staging',
                'username' => 'deploy@shop',
                'environment' => 'staging',
                'tls' => ['mode' => 'require', 'caPath' => null, 'clientCertPath' => null, 'clientKeyPath' => null],
                'ssh' => ['enabled' => true, 'host' => 'jump.example.test', 'port' => 22, 'username' => 'deploy', 'auth' => 'agent', 'keyPath' => null],
                'options' => ['readOnly' => false, 'confirmDestructive' => true, 'statementTimeoutMs' => 120000, 'keepaliveS' => 30, 'initSql' => "SET NAMES utf8mb4; SET time_zone = '+00:00';", 'showSystemObjects' => true, 'redisDb' => 0],
                'savedSecrets' => ['db_password'],
                ...$stamps,
            ]],
            'a Redis connection' => ['connection', [
                'id' => $id,
                'workspaceId' => 'personal',
                'folderId' => null,
                'name' => 'Cache',
                'color' => 'green',
                'sortOrder' => 3,
                'engine' => 'redis',
                'host' => 'cache.shop.example.test',
                'port' => 6380,
                'database' => null,
                'username' => 'default',
                'environment' => 'test',
                'tls' => ['mode' => 'verify_ca', 'caPath' => 'C:\\Users\\kim\\certs\\redis-ca.crt', 'clientCertPath' => null, 'clientKeyPath' => null],
                'ssh' => ['enabled' => false, 'host' => '', 'port' => 22, 'username' => '', 'auth' => 'password', 'keyPath' => null],
                'options' => ['readOnly' => false, 'confirmDestructive' => true, 'statementTimeoutMs' => null, 'keepaliveS' => null, 'initSql' => "CLIENT SETNAME varve\nSELECT 2", 'showSystemObjects' => false, 'redisDb' => 2],
                'savedSecrets' => ['db_password', 'tls_key_passphrase'],
                ...$stamps,
            ]],
            'a saved query' => ['query', [
                'id' => $id, 'workspaceId' => 'personal', 'folderId' => self::FOLDER, 'connectionId' => self::CONNECTION, 'database' => 'shop', 'name' => 'Open orders',
                'sql' => "SELECT o.id, o.total\nFROM orders o\nWHERE o.status = 'open'\nORDER BY o.created_at DESC\nLIMIT 100;",
                'tags' => ['orders', 'daily'], 'sortOrder' => 0, ...$stamps,
            ]],
            'a saved query about passwords, tokens and keys' => ['query', [
                'id' => $id, 'workspaceId' => 'personal', 'folderId' => null, 'connectionId' => null, 'database' => null, 'name' => 'Reset password tokens',
                'sql' => "-- password=hunter2 is only a comment here\n"
                    ."UPDATE users SET password = crypt('x', gen_salt('bf')), api_key = NULL WHERE reset_token = :token;\n"
                    ."ALTER ROLE app PASSWORD 'not-a-real-one';\n"
                    ."SELECT * FROM dblink('host=replica user=report password=report', 'SELECT 1') AS t(one int);\n"
                    ."SELECT 'postgres://report:report@replica.example.test/shop' AS dsn, '{\"password\": \"in a string\"}'::jsonb;",
                'tags' => ['auth', 'secrets'], 'sortOrder' => 1, ...$stamps,
            ]],
            'a saved query holding a certificate and public keys' => ['query', [
                'id' => $id, 'workspaceId' => 'personal', 'folderId' => null, 'connectionId' => self::CONNECTION, 'database' => 'shop', 'name' => 'Trust the new CA',
                'sql' => "INSERT INTO trusted_ca (pem) VALUES ('-----BEGIN CERTIFICATE-----\nMIIBszCCAVmgAwIBAgIUEXAMPLE\n-----END CERTIFICATE-----');\n"
                    ."SELECT pgp_pub_encrypt(note, dearmor('-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nmQENBEXAMPLE\n-----END PGP PUBLIC KEY BLOCK-----')) FROM notes;\n"
                    ."INSERT INTO keys (pub) VALUES ('-----BEGIN PUBLIC KEY-----\nMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEXAMPLE\n-----END PUBLIC KEY-----'), ('ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAEXAMPLE kim@laptop');",
                'tags' => [], 'sortOrder' => 2, ...$stamps,
            ]],
            'saved Redis commands' => ['query', [
                'id' => $id, 'workspaceId' => 'personal', 'folderId' => null, 'connectionId' => null, 'database' => null, 'name' => 'Sessions',
                'sql' => "SCAN 0 MATCH session:* COUNT 100\nTTL session:token:42", 'tags' => ['redis'], 'sortOrder' => 3, ...$stamps,
            ]],
            'a favourite' => ['favourite', ['id' => $id, 'recordId' => self::CONNECTION, 'sortOrder' => 0]],
            'an override of a connection' => ['override', [
                'id' => $id, 'connectionId' => self::CONNECTION, 'username' => 'kim_readonly',
                'tls' => ['caPath' => '/home/kim/certs/shop-ca.pem', 'clientCertPath' => null, 'clientKeyPath' => '/home/kim/certs/client.key'],
                'ssh' => ['username' => 'kim', 'auth' => 'key_file', 'keyPath' => '/home/kim/.ssh/id_ed25519'],
                'savedSecrets' => ['db_password', 'ssh_key_passphrase'],
            ]],
            'preferences' => ['preference', [
                'id' => $id,
                'theme' => 'dark',
                'editor' => ['fontSize' => 13, 'fontFamily' => 'JetBrains Mono', 'tabSize' => 4, 'wordWrap' => false, 'keyBindings' => 'default', 'formatOnSave' => true],
                'grid' => ['pageSize' => 500, 'nullText' => 'NULL', 'dateFormat' => 'yyyy-MM-dd HH:mm:ss'],
                'confirmOnProduction' => true,
                'askForPasswordEveryTime' => false,
                'recentConnectionIds' => [self::CONNECTION],
            ]],
        ];
    }
}
