<?php

declare(strict_types=1);

namespace App\Controller;

use App\Security\OAuth\TokenRevocation;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Routing\Attribute\Route;

/**
 * Token revocation (RFC 7009): `POST /oauth/revoke`, form encoded, with `token`, an optional `token_type_hint` and the
 * `client_id` of the public client the token was issued to. Our apps call it when the user signs out: it signs
 * out the whole device the token belongs to.
 *
 * Always 200 with an empty body once the request names a token and a client, whatever the token is (unknown, expired,
 * already revoked or another client's), so the answer tells nothing about it. Only a request without a token or a
 * client is refused (400 `invalid_request`).
 */
final class OAuthRevocationController extends AbstractController
{
    public const string ROUTE = 'oauth_revoke';
    public const string PATH = '/oauth/revoke';

    public function __construct(private readonly TokenRevocation $revocation)
    {
    }

    #[Route(self::PATH, name: self::ROUTE, methods: ['POST'])]
    public function revoke(Request $request): Response
    {
        $token = $request->request->get('token');
        $clientId = $request->request->get('client_id') ?? $request->getUser();
        if (!\is_string($token) || '' === $token || !\is_string($clientId) || '' === $clientId) {
            return $this->noStore(new JsonResponse([
                'error' => 'invalid_request',
                'error_description' => 'Send the token to revoke as "token" and your client id as "client_id", form encoded.',
            ], Response::HTTP_BAD_REQUEST));
        }
        $hint = $request->request->get('token_type_hint');

        $this->revocation->revoke($token, \is_string($hint) ? $hint : null, $clientId);

        return $this->noStore(new Response('', Response::HTTP_OK));
    }

    /**
     * @template T of Response
     *
     * @param T $response
     *
     * @return T
     */
    private function noStore(Response $response): Response
    {
        $response->headers->set('Cache-Control', 'no-store, private');
        $response->headers->set('Pragma', 'no-cache');

        return $response;
    }
}
