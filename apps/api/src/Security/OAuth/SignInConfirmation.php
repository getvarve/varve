<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use App\Entity\User;
use App\Security\PageHeaders;
use League\Bundle\OAuth2ServerBundle\Event\AuthorizationRequestResolveEvent;
use Symfony\Component\EventDispatcher\Attribute\AsEventListener;
use Symfony\Component\HttpFoundation\RedirectResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\RequestStack;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Routing\Generator\UrlGeneratorInterface;
use Symfony\Component\Security\Csrf\CsrfToken;
use Symfony\Component\Security\Csrf\CsrfTokenManagerInterface;
use Symfony\Component\Security\Http\Authenticator\FormLoginAuthenticator;
use Symfony\Component\Security\Http\Event\LoginSuccessEvent;
use Twig\Environment;

/**
 * Asks the person at the browser before the desktop app is signed in, on /oauth/authorize.
 *
 * The desktop app returns to a port on this computer or to its own scheme, and nothing proves which program asks:
 * any program there can listen on a port, open this address with the desktop app's client id, and wait for the code
 * (RFC 8252 8.6). A browser that is still signed in must therefore never answer such a request by itself, or every
 * program on the computer could sign itself in without anyone noticing.
 *
 * So the request is answered at once only when the sign-in form was just submitted for this very request: the
 * firewall sent the browser from the request to the sign-in page, and the sign-in brought it back. That sign-in
 * marks the one request, by a hash of its query, in the session; the request uses the mark up. Any other request (a
 * live session, a remember-me cookie) gets a page that names the account, with Continue and Cancel. The page posts
 * back to the same address, so the whole request is checked again, with a CSRF token. Cancel sends the app
 * `access_denied`.
 */
final readonly class SignInConfirmation
{
    public const string CSRF_TOKEN_ID = 'oauth_confirm_sign_in';
    public const string CONTINUE = 'continue';
    public const string CANCEL = 'cancel';

    private const string AUTHORIZE_ROUTE = 'oauth2_authorize';
    private const string SIGNED_IN_FOR = '_oauth_signed_in_for';

    public function __construct(
        private RequestStack $requests,
        private UrlGeneratorInterface $urls,
        private CsrfTokenManagerInterface $csrfTokens,
        private Environment $twig,
    ) {
    }

    /** A sign-in on the form that goes on to an authorization request answers that request, and no other. */
    #[AsEventListener(event: LoginSuccessEvent::class, dispatcher: 'security.event_dispatcher.main')]
    public function onLogin(LoginSuccessEvent $event): void
    {
        $request = $event->getRequest();
        if (!$event->getAuthenticator() instanceof FormLoginAuthenticator || !$request->hasSession()) {
            return;
        }
        $session = $request->getSession();
        $session->remove(self::SIGNED_IN_FOR);

        $response = $event->getResponse();
        $target = $response instanceof RedirectResponse ? $response->getTargetUrl() : '';
        if (parse_url($target, \PHP_URL_PATH) === $this->urls->generate(self::AUTHORIZE_ROUTE)) {
            $session->set(self::SIGNED_IN_FOR, self::keyOf(Request::normalizeQueryString((string) parse_url($target, \PHP_URL_QUERY))));
        }
    }

    public function resolve(AuthorizationRequestResolveEvent $event): void
    {
        $request = $this->requests->getMainRequest() ?? throw new \LogicException('An authorization request is always an HTTP request.');
        $user = $event->getUser();
        if (!$user instanceof User) {
            throw new \LogicException(\sprintf('An authorization request needs a %s, got %s.', User::class, get_debug_type($user)));
        }

        if (!$request->isMethod('POST')) {
            $session = $request->getSession();
            if (self::keyOf((string) $request->getQueryString()) === $session->get(self::SIGNED_IN_FOR)) {
                $session->remove(self::SIGNED_IN_FOR);
                $event->resolveAuthorization(AuthorizationRequestResolveEvent::AUTHORIZATION_APPROVED);

                return;
            }
            $event->setResponse($this->page($request, $user));

            return;
        }

        if (!$this->csrfTokens->isTokenValid(new CsrfToken(self::CSRF_TOKEN_ID, $request->request->getString('_csrf_token')))) {
            $event->setResponse($this->page($request, $user, 'This page was open too long or came from another site, so nothing happened. Answer again.', Response::HTTP_FORBIDDEN));

            return;
        }

        switch ($request->request->getString('decision')) {
            case self::CONTINUE:
                // One answer per page: a second post of the same page needs a new token.
                $this->csrfTokens->refreshToken(self::CSRF_TOKEN_ID);
                $event->resolveAuthorization(AuthorizationRequestResolveEvent::AUTHORIZATION_APPROVED);
                break;
            case self::CANCEL:
                $event->resolveAuthorization(AuthorizationRequestResolveEvent::AUTHORIZATION_DENIED);
                break;
            default:
                $event->setResponse($this->page($request, $user, 'Choose Continue or Cancel.', Response::HTTP_BAD_REQUEST));
        }
    }

    private function page(Request $request, User $user, ?string $error = null, int $status = Response::HTTP_OK): Response
    {
        // No referrer: the address of this page holds the app's state and code challenge.
        return PageHeaders::apply(new Response($this->twig->render('security/confirm_sign_in.html.twig', [
            'email' => $user->getEmail(),
            'action' => $request->getRequestUri(),
            'error' => $error,
            'csrf_token_id' => self::CSRF_TOKEN_ID,
        ]), $status), PageHeaders::NO_REFERRER);
    }

    /** An authorization request, as its normalized query (Symfony sorts the parameters). */
    private static function keyOf(string $query): string
    {
        return hash('sha256', $query);
    }
}
