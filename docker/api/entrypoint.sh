#!/bin/sh
set -e

if [ ! -f vendor/autoload_runtime.php ] || [ ! -f composer.lock ] || [ composer.lock -nt vendor/composer/installed.json ] || [ composer.json -nt composer.lock ]; then
    composer install --prefer-dist --no-progress --no-interaction
fi

# OAuth2 signing keys are generated once per environment and never committed. Each file is written next to its place
# and moved there whole, so a stop halfway never leaves a partial key behind.
if [ ! -f config/jwt/private.pem ] || [ ! -f config/jwt/public.pem ]; then
    # Resolve the passphrase the same way Symfony does (real env first, then the .env files). The $ belongs to PHP.
    # shellcheck disable=SC2016
    OAUTH_PASSPHRASE="${OAUTH_PASSPHRASE:-$(php -r 'require "vendor/autoload.php"; (new Symfony\Component\Dotenv\Dotenv())->bootEnv(".env"); echo $_SERVER["OAUTH_PASSPHRASE"];')}"
    export OAUTH_PASSPHRASE
    mkdir -p config/jwt
    if [ ! -f config/jwt/private.pem ]; then
        (
            umask 077
            openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:4096 \
                -aes-256-cbc -pass env:OAUTH_PASSPHRASE -out config/jwt/private.pem.new
        )
        mv config/jwt/private.pem.new config/jwt/private.pem
        rm -f config/jwt/public.pem
    fi
    openssl pkey -in config/jwt/private.pem -passin env:OAUTH_PASSPHRASE -pubout -out config/jwt/public.pem.new
    mv config/jwt/public.pem.new config/jwt/public.pem
fi

if [ "$1" = "frankenphp" ] && [ "${RUN_MIGRATIONS:-0}" = "1" ]; then
    php bin/console doctrine:migrations:migrate --no-interaction --allow-no-migration
    php bin/console app:oauth:bootstrap-clients --no-interaction
fi

exec docker-php-entrypoint "$@"
