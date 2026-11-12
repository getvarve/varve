<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use Doctrine\DBAL\Connection;
use Lcobucci\JWT\Encoding\JoseEncoder;
use Lcobucci\JWT\Signer\Key\InMemory;
use Lcobucci\JWT\Signer\Rsa\Sha256;
use Lcobucci\JWT\Token\Parser;
use Lcobucci\JWT\UnencryptedToken;
use Lcobucci\JWT\Validation\Constraint\SignedWith;
use Lcobucci\JWT\Validation\Validator;
use League\OAuth2\Server\CryptKey;
use Symfony\Component\DependencyInjection\Attribute\Autowire;

/**
 * Reads the tokens this authorization server issued, as presented by a client: a refresh token (a ciphertext of the
 * server's encryption key, {@see TokenCrypt}) or an access token (a JWT signed with the server's private key). Only
 * what the server itself made is read: a refresh token that does not decrypt, or an access token whose signature does
 * not check out, is nothing. Expiry is not checked here.
 */
final readonly class IssuedTokens
{
    public function __construct(
        private TokenCrypt $crypt,
        #[Autowire('%env(resolve:OAUTH_PUBLIC_KEY)%')]
        private string $publicKey,
        private Connection $connection,
    ) {
    }

    /** The id of the refresh token issued together with an access token, if any. */
    public function refreshTokenIssuedWith(string $accessTokenId): ?string
    {
        $id = $this->connection->fetchOne('SELECT identifier FROM oauth2_refresh_token WHERE access_token = :id', ['id' => $accessTokenId]);

        return \is_string($id) ? $id : null;
    }

    public function refreshToken(#[\SensitiveParameter] string $token): ?IssuedToken
    {
        $plaintext = $this->crypt->open($token);
        if (null === $plaintext) {
            return null;
        }

        try {
            $payload = json_decode($plaintext, true, 4, \JSON_THROW_ON_ERROR);
        } catch (\JsonException) {
            return null;
        }
        if (!\is_array($payload)) {
            return null;
        }
        $id = $payload['refresh_token_id'] ?? null;
        $client = $payload['client_id'] ?? null;
        $accessToken = $payload['access_token_id'] ?? null;
        if (!\is_string($id) || !\is_string($client) || !\is_string($accessToken)) {
            return null;
        }

        return new IssuedToken(IssuedToken::REFRESH_TOKEN, $id, $client, $accessToken, IssuedToken::userOf($payload['user_id'] ?? null), IssuedToken::scopesOf($payload['scopes'] ?? null));
    }

    public function accessToken(#[\SensitiveParameter] string $token): ?IssuedToken
    {
        if ('' === $token || 2 !== substr_count($token, '.')) {
            return null;
        }

        try {
            $jwt = new Parser(new JoseEncoder())->parse($token);
        } catch (\Throwable) {
            return null;
        }
        if (!$jwt instanceof UnencryptedToken) {
            return null;
        }

        $key = new CryptKey($this->publicKey, null, false);
        $publicKey = $key->getKeyContents();
        if ('' === $publicKey) {
            throw new \LogicException('The OAuth public key is empty.');
        }
        $signedWithOurKey = new SignedWith(new Sha256(), InMemory::plainText($publicKey, $key->getPassPhrase() ?? ''));
        if (!new Validator()->validate($jwt, $signedWithOurKey)) {
            return null;
        }

        $id = $jwt->claims()->get('jti');
        $audience = $jwt->claims()->get('aud');
        $client = \is_array($audience) ? ($audience[0] ?? null) : null;
        if (!\is_string($id) || '' === $id || !\is_string($client)) {
            return null;
        }

        $user = $jwt->claims()->get('sub');

        return new IssuedToken(IssuedToken::ACCESS_TOKEN, $id, $client, $id, '' === $user ? null : IssuedToken::userOf($user), IssuedToken::scopesOf($jwt->claims()->get('scopes')));
    }
}
