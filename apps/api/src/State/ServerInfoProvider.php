<?php

declare(strict_types=1);

namespace App\State;

use ApiPlatform\Metadata\Operation;
use ApiPlatform\State\ProviderInterface;
use App\ApiResource\ServerInfo;
use App\Billing\BillingSettings;

/**
 * @implements ProviderInterface<ServerInfo>
 */
final readonly class ServerInfoProvider implements ProviderInterface
{
    public function __construct(
        private BillingSettings $billing,
    ) {
    }

    public function provide(Operation $operation, array $uriVariables = [], array $context = []): ServerInfo
    {
        return new ServerInfo(ServerInfo::PRODUCT, ServerInfo::API_VERSION, $this->billing->enabled, $this->billing->enabled ? BillingSettings::TRIAL_DAYS : 0);
    }
}
