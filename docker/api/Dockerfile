# syntax=docker/dockerfile:1
# The two images this one is built from are pinned by digest, so building a release again gives the same image. The
# tag in front of the digest only says which version that is; to move one, take the new digest from
# `docker buildx imagetools inspect <name>:<tag>`.
FROM composer:2@sha256:ca11f643d7791ebdd90a34fc20caf11380d434007ae539a9ea75cd9212e13dca AS composer

FROM dunglas/frankenphp:1.13-php8.5@sha256:667c5fffd348d9c3c260803d75956f1d9aace7e4b7331d8893de6f101d7e0e57 AS base

RUN install-php-extensions \
        pdo_pgsql \
        redis \
        intl \
        opcache \
        zip \
        apcu

ENV COMPOSER_ALLOW_SUPERUSER=1 \
    SERVER_NAME=":80" \
    FRANKENPHP_CONFIG="worker ./public/index.php"

WORKDIR /app

COPY docker/api/Caddyfile /etc/frankenphp/Caddyfile
COPY docker/api/php.ini $PHP_INI_DIR/conf.d/zz-app.ini

FROM base AS dev
ENV APP_ENV=dev
RUN cp $PHP_INI_DIR/php.ini-development $PHP_INI_DIR/php.ini
# Source is bind-mounted in development; vendor is installed by the entrypoint, with this Composer.
COPY --from=composer /usr/bin/composer /usr/bin/composer
COPY docker/api/entrypoint.sh /usr/local/bin/app-entrypoint
ENTRYPOINT ["app-entrypoint"]
CMD ["frankenphp", "run", "--config", "/etc/frankenphp/Caddyfile", "--watch"]

FROM base AS prod
# A port above 1023, which an unprivileged user may listen on without any capability: the image also runs where every
# capability is dropped (cap_drop: ALL, Kubernetes' restricted profile).
ENV APP_ENV=prod \
    SERVER_NAME=":8080"
EXPOSE 8080
RUN cp $PHP_INI_DIR/php.ini-production $PHP_INI_DIR/php.ini
COPY docker/api/php-prod.ini $PHP_INI_DIR/conf.d/zz-prod.ini
COPY apps/api/composer.json apps/api/composer.lock apps/api/symfony.lock ./
# Composer is only lent to the build steps that need it and never becomes part of the image. The download cache stays
# in BuildKit's cache, out of the image too.
RUN --mount=type=bind,from=composer,source=/usr/bin/composer,target=/usr/bin/composer \
    --mount=type=cache,target=/tmp/composer-cache \
    COMPOSER_CACHE_DIR=/tmp/composer-cache \
    composer install --no-dev --no-scripts --no-autoloader --prefer-dist --no-progress --no-interaction
# Only what .dockerignore lets through: no local vendor, var, .env.local or signing keys.
COPY apps/api/ ./
# dump-env compiles the committed .env (development defaults only) into .env.local.php; the real settings come from the
# environment, which wins. post-install-cmd warms the prod cache and installs the bundles' public files.
RUN --mount=type=bind,from=composer,source=/usr/bin/composer,target=/usr/bin/composer \
    composer dump-autoload --classmap-authoritative --no-dev \
    && composer dump-env prod \
    && composer run-script --no-dev post-install-cmd
# The server runs as an unprivileged user. The cache, the OAuth2 signing keys (config/jwt, a volume in production,
# which takes this folder's owner) and Caddy's own state are the only places it writes to. The base image lets
# FrankenPHP bind ports under 1024 through a file capability; a program that carries one cannot even be started where
# that capability is dropped, and port 8080 needs none, so it is taken off.
RUN useradd --uid 1000 --user-group --no-create-home --home-dir /app --shell /usr/sbin/nologin varve \
    && setcap -r /usr/local/bin/frankenphp \
    && mkdir -p config/jwt \
    && chown -R varve:varve var config/jwt /data/caddy /config/caddy
COPY docker/api/entrypoint-prod.sh /usr/local/bin/app-entrypoint
USER varve
# The same check compose.prod.yaml runs: 200 only while the API, its database and its Redis answer.
HEALTHCHECK --interval=10s --timeout=3s --start-period=120s --retries=30 \
    CMD ["curl", "-fsS", "-o", "/dev/null", "http://localhost:8080/health"]
ENTRYPOINT ["app-entrypoint"]
CMD ["frankenphp", "run", "--config", "/etc/frankenphp/Caddyfile"]
