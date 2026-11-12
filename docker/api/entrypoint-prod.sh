#!/bin/sh
set -e

# Nothing starts on a missing or development secret: the image's built-in .env values are public.
php bin/console app:settings:check --no-interaction

# The API container (RUN_MIGRATIONS=1) prepares what every container shares; the worker starts once it is healthy.
if [ "$1" = "frankenphp" ] && [ "${RUN_MIGRATIONS:-0}" = "1" ]; then
    # OAuth2 signing keys, made once and kept in the config/jwt volume, which takes the owner of the image's folder
    # (the unprivileged user this runs as). Each file is written next to its place and moved there whole, so a stop
    # halfway never leaves a partial key behind. The private key is encrypted with OAUTH_PASSPHRASE: changing the
    # passphrase later needs a new key (remove both files), after which every app signs in again.
    if [ ! -w config/jwt ]; then
        echo "config/jwt is not writable by user $(id -u): mount a volume there that this user owns." >&2
        exit 1
    fi
    if [ ! -f config/jwt/private.pem ]; then
        (
            umask 077
            openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096 \
                -aes-256-cbc -pass env:OAUTH_PASSPHRASE -out config/jwt/private.pem.new
        )
        mv config/jwt/private.pem.new config/jwt/private.pem
        rm -f config/jwt/public.pem
    fi
    if [ ! -f config/jwt/public.pem ]; then
        openssl pkey -in config/jwt/private.pem -passin env:OAUTH_PASSPHRASE -pubout -out config/jwt/public.pem.new
        mv config/jwt/public.pem.new config/jwt/public.pem
    fi
    php bin/console doctrine:migrations:migrate --no-interaction --allow-no-migration
    php bin/console app:oauth:bootstrap-clients --no-interaction
fi

exec docker-php-entrypoint "$@"
