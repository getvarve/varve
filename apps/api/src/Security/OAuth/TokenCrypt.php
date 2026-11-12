<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use Defuse\Crypto\Key;
use League\OAuth2\Server\CryptTrait;
use Symfony\Component\DependencyInjection\Attribute\Autowire;

/**
 * Encrypts and decrypts with the authorization server's encryption key, exactly as league/oauth2-server does for the
 * refresh tokens it hands out (which are such ciphertexts). Used to read a refresh token without the token endpoint
 * (revocation, the rotation grace) and to keep a token response encrypted while it waits to be answered again.
 */
final class TokenCrypt
{
    use CryptTrait;

    /** How every ciphertext of the library starts (its version header, in hex). */
    private const string CIPHERTEXT_PREFIX = 'def50200';

    public function __construct(
        #[\SensitiveParameter]
        #[Autowire('%league.oauth2_server.encryption_key%')]
        string $encryptionKey,
        #[Autowire('%league.oauth2_server.encryption_key.type%')]
        string $encryptionKeyType,
    ) {
        $this->setEncryptionKey('defuse' === $encryptionKeyType ? Key::loadFromAsciiSafeString($encryptionKey) : $encryptionKey);
    }

    public function seal(#[\SensitiveParameter] string $plaintext): string
    {
        return $this->encrypt($plaintext);
    }

    /** The plaintext, or null for anything this key did not encrypt (checked cheaply before the key derivation). */
    public function open(#[\SensitiveParameter] string $ciphertext): ?string
    {
        if (!str_starts_with($ciphertext, self::CIPHERTEXT_PREFIX) || !ctype_xdigit($ciphertext)) {
            return null;
        }

        try {
            return $this->decrypt($ciphertext);
        } catch (\InvalidArgumentException|\LogicException) {
            return null;
        }
    }
}
