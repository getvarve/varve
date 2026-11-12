<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use League\Bundle\OAuth2ServerBundle\Manager\ClientManagerInterface;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\HttpKernel\Event\RequestEvent;
use Symfony\Component\HttpKernel\Event\ResponseEvent;
use Symfony\Component\HttpKernel\KernelEvents;

/**
 * The token endpoint's side of {@see RefreshTokenRotation}: answers a refresh with a token that rotated moments ago
 * with the response already issued for it, and records every rotation the endpoint answers. Only for public clients
 * (our apps), which is what the grace is for.
 */
final readonly class RefreshTokenGrace
{
    public const string TOKEN_ROUTE = 'oauth2_token';

    /** The refresh token a request presents, once read, so the response can be recorded against it. */
    private const string PRESENTED = '_oauth_presented_refresh_token';

    public function __construct(
        private IssuedTokens $tokens,
        private RefreshTokenRotation $rotation,
        private ClientManagerInterface $clients,
    ) {
    }

    #[AsEventListener(event: KernelEvents::REQUEST)]
    public function replay(RequestEvent $event): void
    {
        $request = $event->getRequest();
        if (!$event->isMainRequest() || !self::isRefresh($request)) {
            return;
        }

        $presented = $this->tokens->refreshToken($request->request->getString('refresh_token'));
        if (null === $presented || $presented->clientId !== self::clientIdOf($request) || !$this->isPublic($presented->clientId)) {
            return;
        }
        $request->attributes->set(self::PRESENTED, $presented);

        $response = $this->rotation->replay($presented, self::scopeOf($request));
        if (null === $response) {
            return;
        }
        $request->attributes->remove(self::PRESENTED);
        $event->setResponse(new Response($response, Response::HTTP_OK, [
            'Content-Type' => 'application/json; charset=UTF-8',
            'Cache-Control' => 'no-store',
            'Pragma' => 'no-cache',
        ]));
    }

    #[AsEventListener(event: KernelEvents::RESPONSE)]
    public function record(ResponseEvent $event): void
    {
        $request = $event->getRequest();
        $presented = $request->attributes->get(self::PRESENTED);
        $response = $event->getResponse();
        if (!$event->isMainRequest() || !$presented instanceof IssuedToken || Response::HTTP_OK !== $response->getStatusCode()) {
            return;
        }

        $content = (string) $response->getContent();
        try {
            $body = json_decode($content, true, 4, \JSON_THROW_ON_ERROR);
        } catch (\JsonException) {
            return;
        }
        $accessToken = \is_array($body) ? ($body['access_token'] ?? null) : null;
        $issued = \is_string($accessToken) ? $this->tokens->accessToken($accessToken) : null;
        if (null === $issued) {
            return;
        }
        $successor = $this->tokens->refreshTokenIssuedWith($issued->id);
        if (null !== $successor) {
            $this->rotation->rotated($presented, self::scopeOf($request), $content, $successor);
        }
    }

    public static function isRefresh(Request $request): bool
    {
        return self::TOKEN_ROUTE === $request->attributes->get('_route')
            && $request->isMethod('POST')
            && 'refresh_token' === $request->request->get('grant_type')
            && \is_string($request->request->get('refresh_token'));
    }

    /** Where league reads the client from: the body, or else HTTP Basic authentication. */
    public static function clientIdOf(Request $request): ?string
    {
        $clientId = $request->request->get('client_id');

        return \is_string($clientId) ? $clientId : $request->getUser();
    }

    private static function scopeOf(Request $request): ?string
    {
        $scope = $request->request->get('scope');

        return \is_string($scope) ? $scope : null;
    }

    private function isPublic(string $clientId): bool
    {
        $client = $this->clients->find($clientId);

        return null !== $client && \in_array($client->getSecret(), [null, ''], true);
    }
}
