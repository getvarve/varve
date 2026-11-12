<?php

declare(strict_types=1);

namespace App\Controller;

use App\Security\OAuth\AuthorizationServerMetadata;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\Routing\Attribute\Route;

/**
 * The metadata an OAuth2 client reads to find out how to sign in here: the authorization server (RFC 8414) and the
 * resource it protects, the API (RFC 9728). Public, and the same for everyone.
 */
final class OAuthMetadataController extends AbstractController
{
    public const string PROTECTED_RESOURCE_PATH = '/.well-known/oauth-protected-resource';

    public function __construct(private readonly AuthorizationServerMetadata $metadata)
    {
    }

    #[Route(AuthorizationServerMetadata::PATH, name: 'oauth_authorization_server_metadata', methods: ['GET'])]
    public function authorizationServer(): JsonResponse
    {
        return $this->document($this->metadata->document());
    }

    #[Route(self::PROTECTED_RESOURCE_PATH, name: 'oauth_protected_resource_metadata', methods: ['GET'])]
    public function api(): JsonResponse
    {
        return $this->document($this->metadata->apiResource());
    }

    /** @param array<string, mixed> $document */
    private function document(array $document): JsonResponse
    {
        $response = new JsonResponse($document);
        $response->setPublic();
        $response->setMaxAge(3600);

        return $response;
    }
}
