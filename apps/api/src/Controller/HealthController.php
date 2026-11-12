<?php

declare(strict_types=1);

namespace App\Controller;

use Doctrine\DBAL\Connection;
use Symfony\Component\HttpFoundation\JsonResponse;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Routing\Attribute\Route;

/**
 * Liveness and dependency check used by Docker and load balancers: 200 while every dependency answers, 503 when one
 * does not.
 */
final readonly class HealthController
{
    public function __construct(
        private Connection $connection,
    ) {
    }

    #[Route('/health', name: 'health', methods: ['GET'])]
    public function __invoke(): JsonResponse
    {
        $checks = [
            'database' => $this->check(fn () => $this->connection->executeQuery('SELECT 1')->fetchOne()),
        ];

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
