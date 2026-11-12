# syntax=docker/dockerfile:1
FROM dunglas/frankenphp:1.13-php8.5 AS base

RUN install-php-extensions \
        pdo_pgsql \
        redis \
        intl \
        opcache \
        zip \
        apcu

COPY --from=composer:2 /usr/bin/composer /usr/bin/composer

ENV COMPOSER_ALLOW_SUPERUSER=1 \
    SERVER_NAME=":80" \
    FRANKENPHP_CONFIG="worker ./public/index.php"

WORKDIR /app

COPY docker/api/Caddyfile /etc/frankenphp/Caddyfile
COPY docker/api/php.ini $PHP_INI_DIR/conf.d/zz-app.ini

FROM base AS dev
ENV APP_ENV=dev
RUN cp $PHP_INI_DIR/php.ini-development $PHP_INI_DIR/php.ini
# Source is bind-mounted in development; vendor is installed by the entrypoint.
COPY docker/api/entrypoint.sh /usr/local/bin/app-entrypoint
ENTRYPOINT ["app-entrypoint"]
CMD ["frankenphp", "run", "--config", "/etc/frankenphp/Caddyfile", "--watch"]

FROM base AS prod
ENV APP_ENV=prod
RUN cp $PHP_INI_DIR/php.ini-production $PHP_INI_DIR/php.ini
COPY apps/api/composer.json apps/api/composer.lock apps/api/symfony.lock ./
RUN composer install --no-dev --no-scripts --no-autoloader --prefer-dist --no-progress
COPY apps/api/ ./
RUN composer dump-autoload --classmap-authoritative --no-dev \
    && composer dump-env prod \
    && composer run-script --no-dev post-install-cmd
COPY docker/api/entrypoint-prod.sh /usr/local/bin/app-entrypoint
ENTRYPOINT ["app-entrypoint"]
CMD ["frankenphp", "run", "--config", "/etc/frankenphp/Caddyfile"]
