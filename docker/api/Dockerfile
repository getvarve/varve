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
COPY docker/api/php-prod.ini $PHP_INI_DIR/conf.d/zz-prod.ini
COPY apps/api/composer.json apps/api/composer.lock apps/api/symfony.lock ./
# The download cache stays in BuildKit's cache, out of the image.
RUN --mount=type=cache,target=/tmp/composer-cache \
    COMPOSER_CACHE_DIR=/tmp/composer-cache \
    composer install --no-dev --no-scripts --no-autoloader --prefer-dist --no-progress --no-interaction
# Only what .dockerignore lets through: no local vendor, var, .env.local or signing keys.
COPY apps/api/ ./
# dump-env compiles the committed .env (development defaults only) into .env.local.php; the real settings come from the
# environment, which wins. post-install-cmd warms the prod cache and installs the bundles' public files.
RUN composer dump-autoload --classmap-authoritative --no-dev \
    && composer dump-env prod \
    && composer run-script --no-dev post-install-cmd
# The server runs as an unprivileged user. FrankenPHP still listens on port 80, which the file capability allows; the
# cache, the OAuth2 signing keys (config/jwt, a volume in production, which takes this folder's owner) and Caddy's own
# state are the only places it writes to.
RUN useradd --uid 1000 --user-group --no-create-home --home-dir /app --shell /usr/sbin/nologin varve \
    && setcap CAP_NET_BIND_SERVICE=+eip /usr/local/bin/frankenphp \
    && mkdir -p config/jwt \
    && chown -R varve:varve var config/jwt /data/caddy /config/caddy
COPY docker/api/entrypoint-prod.sh /usr/local/bin/app-entrypoint
USER varve
# The same check compose.prod.yaml runs: 200 only while the API and its database answer.
HEALTHCHECK --interval=10s --timeout=3s --start-period=120s --retries=30 \
    CMD ["curl", "-fsS", "-o", "/dev/null", "http://localhost/health"]
ENTRYPOINT ["app-entrypoint"]
CMD ["frankenphp", "run", "--config", "/etc/frankenphp/Caddyfile"]
