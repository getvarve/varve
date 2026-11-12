#!/bin/sh
set -e

# Nothing starts on a missing or development secret: the image's built-in .env values are public.
php bin/console app:settings:check --no-interaction

private=config/jwt/private.pem
public=config/jwt/public.pem

# The public half of the private key, as the passphrase of this container opens it. Prints nothing when it does not.
public_of_private() {
    openssl pkey -in "$private" -passin env:OAUTH_PASSPHRASE -pubout 2>/dev/null
}

# The API container (RUN_MIGRATIONS=1) prepares what every container shares; the worker starts once it is healthy.
if [ "$1" = "frankenphp" ] && [ "${RUN_MIGRATIONS:-0}" = "1" ]; then
    # OAuth2 signing keys, made once and kept in the config/jwt volume, which takes the owner of the image's folder
    # (the unprivileged user this runs as). The private key is encrypted with OAUTH_PASSPHRASE: changing the
    # passphrase later needs a new key (remove both files), after which every app signs in again.
    if [ ! -w config/jwt ]; then
        echo "config/jwt is not writable by user $(id -u): mount a volume there that this user owns." >&2
        exit 1
    fi
    # Each file is written under a name of this container's own and only then given its real name, whole, so a stop
    # halfway never leaves a partial key behind and two containers that start together never write into one file.
    if [ ! -f "$private" ]; then
        new="$(mktemp config/jwt/private.pem.XXXXXX)"
        openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096 -aes-256-cbc -pass env:OAUTH_PASSPHRASE -out "$new"
        # ln, unlike mv, refuses to replace a key another container published in the meantime: that one stays.
        if ! ln "$new" "$private" 2>/dev/null; then
            echo "Another container made the signing key first; this one uses it."
        fi
        rm -f "$new"
    fi
    # The public key is only ever the public half of the private key, so it is written again whenever it is missing
    # or belongs to another key. Two containers doing this at once write the same content.
    new="$(mktemp config/jwt/public.pem.XXXXXX)"
    if ! public_of_private > "$new"; then
        rm -f "$new"
        echo "config/jwt/private.pem does not open with OAUTH_PASSPHRASE. Set the passphrase the key was made with; or, to start over with a new key (every app then signs in again), remove private.pem and public.pem from the volume." >&2
        exit 1
    fi
    if cmp -s "$new" "$public"; then
        rm -f "$new"
    else
        chmod 644 "$new"
        mv "$new" "$public"
    fi
fi

# Every container, on every start: a token signed with a private key that public.pem does not belong to is refused
# by every API call (401), and a passphrase that does not open the key fails every sign-in (500), neither with a
# word about the cause. So neither starts.
if [ ! -f "$private" ] || [ ! -f "$public" ]; then
    echo "config/jwt holds no signing keys. The API container with RUN_MIGRATIONS=1 makes them on its first start; start that one first, and mount its config/jwt volume in this container." >&2
    exit 1
fi
if ! public_of_private | cmp -s - "$public"; then
    echo "config/jwt: OAUTH_PASSPHRASE does not open private.pem, or public.pem is not the public half of private.pem. Give every container the passphrase the key was made with and the same config/jwt volume." >&2
    exit 1
fi

if [ "$1" = "frankenphp" ] && [ "${RUN_MIGRATIONS:-0}" = "1" ]; then
    php bin/console doctrine:migrations:migrate --no-interaction --allow-no-migration
    php bin/console app:oauth:bootstrap-clients --no-interaction
fi

exec docker-php-entrypoint "$@"
