<?php

declare(strict_types=1);

namespace App\Security;

use App\Deployment\PublicAddresses;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;
use Symfony\Component\HttpFoundation\RedirectResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\HttpKernel\Event\RequestEvent;
use Symfony\Component\HttpKernel\KernelEvents;
use Symfony\Component\Routing\Generator\UrlGeneratorInterface;
use Symfony\Component\Security\Csrf\CsrfToken;
use Symfony\Component\Security\Csrf\CsrfTokenManagerInterface;
use Symfony\Component\Security\Http\Event\LogoutEvent;
use Twig\Environment;

/**
 * Lets only a deliberate sign-out end the browser's sign-in (the session and the remember-me cookie of the firewall
 * `main`), so another site cannot sign people out with an image or a form of its own.
 *
 * Runs before the firewall, whose logout listener would otherwise end the sign-in on any request to /logout:
 * - GET shows a page with a Sign out button (or, when this browser is not signed in, goes straight on to where a
 *   sign-out ends). A link or a navigation never signs anyone out by itself.
 * - POST goes on to the firewall when it comes from Varve: with the page's CSRF token (always enough, also behind a
 *   proxy that hides the scheme), or with an Origin header of this server or of the web client (which signs out by
 *   submitting a form here, without a token). Anything else gets the page again with a 403, and nothing happens.
 *
 * A sign-out ends at the web client when this server has one, else on a page that says the browser is signed out.
 * The firewall's own `logout.target` cannot send the browser to the web client: Symfony only redirects to the host of
 * the request there, and the web client usually has another one.
 */
#[AsEventListener(event: KernelEvents::REQUEST, method: '__invoke', priority: self::PRIORITY)]
#[AsEventListener(event: LogoutEvent::class, method: 'afterSignOut', dispatcher: 'security.event_dispatcher.main', priority: -64)]
final readonly class SignOutGuard
{
    public const string PATH = '/logout';
    public const string CSRF_TOKEN_ID = 'logout';
    public const string REMEMBER_ME_COOKIE = 'REMEMBERME';

    /** Above the firewall (8), below the router (32) and the session (128). */
    private const int PRIORITY = 9;

    public function __construct(
        private CsrfTokenManagerInterface $csrfTokens,
        private Environment $twig,
        private PublicAddresses $addresses,
        private UrlGeneratorInterface $urls,
    ) {
    }

    public function __invoke(RequestEvent $event): void
    {
        $request = $event->getRequest();
        if (!$event->isMainRequest() || self::PATH !== $request->getPathInfo()) {
            return;
        }

        if ($request->isMethod('POST')) {
            if ($this->fromVarve($request)) {
                return;
            }
            // Without the session cookie no token can match, and a page here would start a new session whose cookie
            // replaces the browser's: send it to the sign-out page, a navigation that carries its cookies.
            $event->setResponse($request->hasPreviousSession()
                ? $this->page($request, 'This sign-out came from another site, so nothing happened. Sign out here if you meant to.')
                : new RedirectResponse($request->getBaseUrl().self::PATH, Response::HTTP_SEE_OTHER));

            return;
        }

        if (!$request->isMethod('GET') && !$request->isMethod('HEAD')) {
            $event->setResponse(new Response('', Response::HTTP_METHOD_NOT_ALLOWED, ['Allow' => 'GET, HEAD, POST']));

            return;
        }

        if (!$request->hasPreviousSession() && !$request->cookies->has(self::REMEMBER_ME_COOKIE)) {
            $event->setResponse(new RedirectResponse($this->afterSignOutUrl()));

            return;
        }

        $event->setResponse($this->page($request));
    }

    /**
     * After the firewall's own logout listeners: the default one set a redirect to "/", and the others added the
     * cookies that clear the session and the remember-me cookie to that response, so only its target changes.
     */
    public function afterSignOut(LogoutEvent $event): void
    {
        $response = $event->getResponse();
        if ($response instanceof RedirectResponse) {
            $response->setTargetUrl($this->afterSignOutUrl());

            return;
        }

        $redirect = new RedirectResponse($this->afterSignOutUrl());
        foreach ($response?->headers->getCookies() ?? [] as $cookie) {
            $redirect->headers->setCookie($cookie);
        }
        $event->setResponse($redirect);
    }

    /** The web client when this server has one, else the page that says the browser is signed out. */
    private function afterSignOutUrl(): string
    {
        return $this->addresses->web() ?? $this->urls->generate('app_signed_out', [], UrlGeneratorInterface::ABSOLUTE_URL);
    }

    /**
     * The page's CSRF token is always enough (it is bound to the session). An Origin of this host or of the web client
     * is the way without one: the web client posts here from its own page, which has no token.
     */
    private function fromVarve(Request $request): bool
    {
        // Only with a session: reading a token starts one.
        if ($request->hasPreviousSession() && $this->csrfTokens->isTokenValid(new CsrfToken(self::CSRF_TOKEN_ID, $request->request->getString('_csrf_token')))) {
            return true;
        }
        $origin = $request->headers->get('Origin');

        return null !== $origin && \in_array($origin, array_filter([$request->getSchemeAndHttpHost(), $this->addresses->web()]), true);
    }

    private function page(Request $request, ?string $error = null): Response
    {
        return PageHeaders::apply(new Response($this->twig->render('security/logout.html.twig', [
            'action' => $request->getBaseUrl().self::PATH,
            'error' => $error,
            'csrf_token_id' => self::CSRF_TOKEN_ID,
        ]), null === $error ? Response::HTTP_OK : Response::HTTP_FORBIDDEN));
    }
}
