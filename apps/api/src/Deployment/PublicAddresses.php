<?php

declare(strict_types=1);

namespace App\Deployment;

use Symfony\Component\DependencyInjection\Attribute\Autowire;

/**
 * The public addresses this server answers on, in one place:
 * - API_PUBLIC_URL: the API, the sign-in pages, the token endpoint, the metadata, the issuer of every token and the
 *   links in account mail;
 * - WEB_APP_URL: the web client, once it has a deployment. Empty: this server has no web client.
 */
final readonly class PublicAddresses
{
    private string $api;
    private ?string $web;

    public function __construct(
        #[Autowire(env: 'API_PUBLIC_URL')]
        string $api,
        #[Autowire(env: 'WEB_APP_URL')]
        string $web,
    ) {
        $this->api = rtrim(trim($api), '/');
        if ('' === $this->api) {
            throw new \InvalidArgumentException('API_PUBLIC_URL is empty.');
        }
        $web = rtrim(trim($web), '/');
        $this->web = '' === $web ? null : $web;
    }

    public function api(): string
    {
        return $this->api;
    }

    /** The web client's address, or null while this server has none. */
    public function web(): ?string
    {
        return $this->web;
    }
}
