<?php

declare(strict_types=1);

namespace App\Security;

use App\Account\SignUpSettings;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;
use Symfony\Component\HttpFoundation\RedirectResponse;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\HttpKernel\Event\RequestEvent;
use Symfony\Component\HttpKernel\KernelEvents;
use Symfony\Component\Security\Csrf\CsrfToken;
use Symfony\Component\Security\Csrf\CsrfTokenManagerInterface;
use Twig\Environment;

/**
 * Checks the sign-in form's CSRF token before the firewall sees the post. The firewall checks it too, but only as part
 * of the sign-in attempt: a post with a wrong token there is a failed sign-in, which clears the browser's remember-me
 * cookie and counts against the login limit for the posted address from that browser's address. So another site
 * could sign people out, or lock their form, by posting to /login. Here such a post gets the sign-in page again with
 * a 403, or, when it came without the session cookie (as a post from another site does), a 303 to the sign-in page,
 * so that no new session cookie replaces the browser's either.
 */
#[AsEventListener(event: KernelEvents::REQUEST, priority: self::PRIORITY)]
final readonly class SignInGuard
{
    public const string ROUTE = 'app_login';
    public const string CSRF_TOKEN_ID = 'authenticate';

    /** Above the firewall (8), below the router (32). */
    private const int PRIORITY = 9;

    public function __construct(
        private CsrfTokenManagerInterface $csrfTokens,
        private Environment $twig,
        private SignUpSettings $signUp,
    ) {
    }

    public function __invoke(RequestEvent $event): void
    {
        $request = $event->getRequest();
        if (!$event->isMainRequest() || self::ROUTE !== $request->attributes->get('_route') || !$request->isMethod('POST')) {
            return;
        }
        // Without the session cookie no token can match, and rendering the page would start a new session whose cookie
        // replaces the browser's: send it to the sign-in page, a navigation that carries its cookies.
        if (!$request->hasPreviousSession()) {
            $event->setResponse(new RedirectResponse($request->getBaseUrl().$request->getPathInfo(), Response::HTTP_SEE_OTHER));

            return;
        }
        if ($this->csrfTokens->isTokenValid(new CsrfToken(self::CSRF_TOKEN_ID, $request->request->getString('_csrf_token')))) {
            return;
        }

        $event->setResponse(PageHeaders::apply(new Response($this->twig->render('security/login.html.twig', [
            'last_username' => $request->request->getString('_username'),
            'error' => null,
            'unconfirmed' => false,
            'sign_up' => $this->signUp->open,
            'notice' => 'This page was open too long or came from another site, so nothing happened. Sign in again.',
        ]), Response::HTTP_FORBIDDEN)));
    }
}
