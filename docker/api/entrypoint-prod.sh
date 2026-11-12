#!/bin/sh
set -e

# Nothing starts on a missing or development secret: the image's built-in .env values are public.
php bin/console app:settings:check --no-interaction

# The API container (RUN_MIGRATIONS=1) prepares what every container shares.
if [ "$1" = "frankenphp" ] && [ "${RUN_MIGRATIONS:-0}" = "1" ]; then
    php bin/console doctrine:migrations:migrate --no-interaction --allow-no-migration
fi

exec docker-php-entrypoint "$@"
