<?php

declare(strict_types=1);

namespace App\Controller;

use Doctrine\DBAL\Connection;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Routing\Attribute\Route;

/**
 * Liveness and dependency check used by Docker and load balancers: 200 while every dependency answers, 503 when one
 * does not.
 *
 * `redis` is checked where the server uses Redis, which is production: sign-in sessions, the limits and the refresh
 * token grace live there, so without it every sign-in fails. Development and tests keep those in files and have no
 * such check. The PING waits a second at most (app.health_redis, config/packages/framework.yaml).
 */
final readonly class HealthController
{
    public function __construct(
        private Connection $connection,
        #[Autowire(service: 'app.health_redis')]
        private ?\Redis $redis = null,
    ) {
    }

    #[Route('/health', name: 'health', methods: ['GET'])]
    public function __invoke(): JsonResponse
    {
        $checks = [
            'database' => $this->check(fn () => $this->connection->executeQuery('SELECT 1')->fetchOne()),
        ];
        if (null !== $this->redis) {
            $redis = $this->redis;
            $checks['redis'] = $this->check(static fn () => $redis->ping());
        }

        $healthy = !\in_array(false, $checks, true);

        return new JsonResponse(
            ['status' => $healthy ? 'ok' : 'degraded', 'checks' => array_map(static fn (bool $ok): string => $ok ? 'ok' : 'failing', $checks)],
            $healthy ? Response::HTTP_OK : Response::HTTP_SERVICE_UNAVAILABLE,
        );
    }

    private function check(callable $probe): bool
    {
        try {
            $probe();

            return true;
        } catch (\Throwable) {
            return false;
        }
    }
}
