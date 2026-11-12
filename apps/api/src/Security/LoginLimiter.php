<?php

declare(strict_types=1);

namespace App\Security;

use App\Account\ClientAddress;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\HttpFoundation\RateLimiter\AbstractRequestRateLimiter;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\RateLimiter\LimiterInterface;
use Symfony\Component\RateLimiter\RateLimiterFactoryInterface;
use Symfony\Component\Security\Http\SecurityRequestAttributes;

/**
 * How many wrong passwords the sign-in form takes before it locks (config/packages/rate_limiter.yaml):
 * - 5 in 15 minutes for one account from one network address;
 * - 25 in 15 minutes from one network address, whatever the account;
 * - 20 an hour for one account, whatever the network address.
 *
 * A network address is an IPv4 address or an IPv6 /64 ({@see ClientAddress}): a home line has a whole /64, so
 * counting per exact address would give it a fresh set of attempts with every new source address. The limit per
 * account alone is for someone with many networks. A sign-in that succeeds clears only the first: the other two
 * bound what one network, or everyone together, may try, whatever else happens.
 *
 * Only failed attempts count (the firewall peeks before, and takes one after a failure). The keys are hashed with
 * the application secret, so the store holds no addresses.
 */
final class LoginLimiter extends AbstractRequestRateLimiter
{
    public function __construct(
        #[Autowire(service: 'limiter.login_address')]
        private readonly RateLimiterFactoryInterface $perAddress,
        #[Autowire(service: 'limiter.login_account_address')]
        private readonly RateLimiterFactoryInterface $perAccountAndAddress,
        #[Autowire(service: 'limiter.login_account')]
        private readonly RateLimiterFactoryInterface $perAccount,
        #[\SensitiveParameter]
        #[Autowire('%kernel.secret%')]
        private readonly string $secret,
    ) {
    }

    public function reset(Request $request): void
    {
        $this->accountAndAddress($request)->reset();
    }

    protected function getLimiters(Request $request): array
    {
        return [
            $this->perAddress->create($this->hash(ClientAddress::key($request->getClientIp()))),
            $this->accountAndAddress($request),
            $this->perAccount->create($this->hash(self::accountOf($request))),
        ];
    }

    private function accountAndAddress(Request $request): LimiterInterface
    {
        return $this->perAccountAndAddress->create($this->hash(self::accountOf($request).'|'.ClientAddress::key($request->getClientIp())));
    }

    /** The address typed on the form, as the account is looked up: trimmed, in lower case. */
    private static function accountOf(Request $request): string
    {
        $username = $request->attributes->get(SecurityRequestAttributes::LAST_USERNAME, '');
        $username = \is_string($username) ? trim($username) : '';

        return 1 === preg_match('//u', $username) ? mb_strtolower($username) : strtolower($username);
    }

    private function hash(string $key): string
    {
        return hash_hmac('sha256', $key, $this->secret);
    }
}
