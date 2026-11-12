<?php

declare(strict_types=1);

namespace App\Device;

use App\Entity\Device;
use App\Entity\DeviceSession;
use App\Repository\UserRepository;
use App\Security\OAuth\IssuedTokens;
use App\Security\OAuth\RefreshTokenGrace;
use Doctrine\DBAL\Connection;
use Doctrine\ORM\EntityManagerInterface;
use League\Bundle\OAuth2ServerBundle\Event\TokenRequestResolveEvent;
use League\Bundle\OAuth2ServerBundle\Model\AccessToken;
use League\Bundle\OAuth2ServerBundle\Model\Client;
use League\Bundle\OAuth2ServerBundle\OAuth2Events;
use Psr\Clock\ClockInterface;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\RequestStack;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\HttpKernel\Event\RequestEvent;
use Symfony\Component\HttpKernel\KernelEvents;

/**
 * Ties every token the token endpoint issues to a device:
 * - exchanging an authorization code starts a device, named by the app in the same request ({@see DeviceDescription});
 *   without a valid description the code is not exchanged at all (400 `invalid_request`), and stays usable;
 * - a refresh continues the device of the refresh token it presents, and marks it seen. A refresh whose old tokens
 *   belong to no device (it was signed out in between) gets `invalid_grant`, and what it was just issued is revoked.
 *
 * A token without a device does not work on the API ({@see DeviceGuard}), so a failure here locks out rather than in.
 */
final readonly class DeviceSignIn
{
    private const string DESCRIPTION = '_device_description';

    public function __construct(
        private RequestStack $requests,
        private IssuedTokens $tokens,
        private EntityManagerInterface $entityManager,
        private Connection $connection,
        private UserRepository $users,
        private Devices $devices,
        private ClockInterface $clock,
    ) {
    }

    /** Below the router (32): checks the description before the token endpoint spends the code. */
    #[AsEventListener(event: KernelEvents::REQUEST, priority: 4)]
    public function checkDescription(RequestEvent $event): void
    {
        $request = $event->getRequest();
        if (!$event->isMainRequest() || RefreshTokenGrace::TOKEN_ROUTE !== $request->attributes->get('_route') || !$request->isMethod('POST')) {
            return;
        }

        $grant = $request->request->get('grant_type');
        if ('authorization_code' === $grant) {
            $description = DeviceDescription::fromRequest($request);
            if (\is_string($description)) {
                $event->setResponse(self::invalidRequest($description));

                return;
            }
            $request->attributes->set(self::DESCRIPTION, $description);
        } elseif ('refresh_token' === $grant && $request->request->has(DeviceDescription::APP_VERSION) && null === DeviceDescription::appVersion($request->request->get(DeviceDescription::APP_VERSION))) {
            $event->setResponse(self::invalidRequest(DeviceDescription::appVersionProblem()));
        }
    }

    #[AsEventListener(event: OAuth2Events::TOKEN_REQUEST_RESOLVE)]
    public function onTokens(TokenRequestResolveEvent $event): void
    {
        $request = $this->requests->getMainRequest();
        $response = $event->getResponse();
        if (null === $request || Response::HTTP_OK !== $response->getStatusCode()) {
            return;
        }
        $issued = $this->tokens->accessToken(self::accessTokenOf($response));
        if (null === $issued) {
            throw new \LogicException('The token endpoint answered 200 without an access token of this server.');
        }

        $grant = $request->request->get('grant_type');
        if ('authorization_code' === $grant) {
            $description = $request->attributes->get(self::DESCRIPTION);
            if (!$description instanceof DeviceDescription) {
                throw new \LogicException('A code exchange always carries a checked device description.');
            }
            $this->start($issued->id, $issued->clientId, $issued->userIdentifier, $description);

            return;
        }

        if ('refresh_token' === $grant && !$this->continue($request, $issued->id)) {
            $this->devices->signOutByAccessToken($issued->id);
            $event->setResponse(new JsonResponse(
                ['error' => 'invalid_grant', 'error_description' => 'This device was signed out. Sign in again.'],
                Response::HTTP_BAD_REQUEST,
                ['Cache-Control' => 'no-store', 'Pragma' => 'no-cache'],
            ));
        }
    }

    private function start(string $accessToken, string $clientId, ?string $userIdentifier, DeviceDescription $description): void
    {
        $user = null === $userIdentifier ? null : $this->users->findOneByEmail($userIdentifier);
        if (null === $user) {
            throw new \LogicException('An authorization code is always issued to a user.');
        }
        $now = $this->clock->now();
        $client = $this->entityManager->getReference(Client::class, $clientId) ?? throw new \LogicException('A token is always issued to a client.');
        $token = $this->entityManager->getReference(AccessToken::class, $accessToken) ?? throw new \LogicException('The access token was just stored.');
        $device = new Device($user, $client, $description->name, $description->platform, $description->appVersion, $now);
        $this->entityManager->persist($device);
        $this->entityManager->persist(new DeviceSession($token, $device, $now));
        $this->entityManager->flush();
    }

    /** Adds the new access token to the device of the refresh token presented; false when that device is gone. */
    private function continue(Request $request, string $accessToken): bool
    {
        $presented = $this->tokens->refreshToken($request->request->getString('refresh_token'));
        if (null === $presented) {
            return false;
        }
        $device = $this->connection->fetchOne('SELECT device_id FROM device_session WHERE access_token = :id', ['id' => $presented->accessTokenId]);
        if (!\is_string($device)) {
            return false;
        }

        $now = $this->clock->now()->format('Y-m-d H:i:s');
        $this->connection->transactional(function () use ($request, $accessToken, $device, $now): void {
            $this->connection->executeStatement(
                'INSERT INTO device_session (access_token, device_id, created_at) VALUES (:token, :device, :now)',
                ['token' => $accessToken, 'device' => $device, 'now' => $now],
            );
            $version = DeviceDescription::appVersion($request->request->get(DeviceDescription::APP_VERSION));
            $this->connection->executeStatement(
                'UPDATE device SET last_seen_at = :now, app_version = COALESCE(:version, app_version) WHERE id = :device',
                ['now' => $now, 'version' => $version, 'device' => $device],
            );
        });

        return true;
    }

    private static function accessTokenOf(Response $response): string
    {
        try {
            $body = json_decode((string) $response->getContent(), true, 4, \JSON_THROW_ON_ERROR);
        } catch (\JsonException) {
            return '';
        }
        $token = \is_array($body) ? ($body['access_token'] ?? null) : null;

        return \is_string($token) ? $token : '';
    }

    private static function invalidRequest(string $description): JsonResponse
    {
        return new JsonResponse(
            ['error' => 'invalid_request', 'error_description' => $description],
            Response::HTTP_BAD_REQUEST,
            ['Cache-Control' => 'no-store', 'Pragma' => 'no-cache'],
        );
    }
}
