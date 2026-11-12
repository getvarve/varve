<?php

declare(strict_types=1);

namespace App\Billing;

use Symfony\Component\DependencyInjection\Attribute\Autowire;

/**
 * Whether this server bills its organizations. Off unless VARVE_BILLING is 1: a self-hosted server never bills and
 * never blocks anyone. Varve Cloud turns it on.
 */
final readonly class BillingSettings
{
    public const int TRIAL_DAYS = 14;

    public function __construct(
        #[Autowire(env: 'bool:VARVE_BILLING')]
        public bool $enabled,
    ) {
    }
}
