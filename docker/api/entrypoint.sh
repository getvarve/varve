#!/bin/sh
set -e

if [ ! -f vendor/autoload_runtime.php ] || [ ! -f composer.lock ] || [ composer.lock -nt vendor/composer/installed.json ] || [ composer.json -nt composer.lock ]; then
    composer install --prefer-dist --no-progress --no-interaction
fi

if [ "$1" = "frankenphp" ] && [ "${RUN_MIGRATIONS:-0}" = "1" ]; then
    php bin/console doctrine:migrations:migrate --no-interaction --allow-no-migration
fi

exec docker-php-entrypoint "$@"
