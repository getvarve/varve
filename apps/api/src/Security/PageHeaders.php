<?php

declare(strict_types=1);

namespace App\Security;

use Symfony\Component\HttpFoundation\Response;

/**
 * Headers of the account pages (sign in, sign out, sign up, confirm, reset): never inside someone else's page
 * (clickjacking), never cached. Same-origin referrers by default, so a form's POST carries an Origin; none at all on a
 * page whose address holds the secret of a mail link.
 */
final class PageHeaders
{
    public const string SAME_ORIGIN = 'same-origin';
    public const string NO_REFERRER = 'no-referrer';

    /**
     * @template T of Response
     *
     * @param T $response
     *
     * @return T
     */
    public static function apply(Response $response, string $referrerPolicy = self::SAME_ORIGIN): Response
    {
        $response->headers->set('X-Frame-Options', 'DENY');
        $response->headers->set('Content-Security-Policy', "frame-ancestors 'none'");
        $response->headers->set('Cache-Control', 'no-store, private');
        $response->headers->set('Referrer-Policy', $referrerPolicy);

        return $response;
    }
}
