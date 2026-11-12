<?php

declare(strict_types=1);

namespace App\Security\OAuth;

use Symfony\Component\EventDispatcher\Attribute\AsEventListener;
use Symfony\Component\HttpFoundation\RedirectResponse;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpKernel\Event\RequestEvent;
use Symfony\Component\HttpKernel\KernelEvents;
use Symfony\Component\Routing\Generator\UrlGeneratorInterface;
use Symfony\Component\Security\Http\Authenticator\FormLoginAuthenticator;
use Symfony\Component\Security\Http\Event\LoginSuccessEvent;

/**
 * `prompt=login` on an authorization request (as OpenID Connect defines it): the user types the password again for
 * this request, whatever session or remember-me cookie the browser holds. Our apps send it on their authorization
 * requests after the user signed out of them, until a sign-in completes (the desktop app cannot end the browser's
 * sign-in itself, and the web client's sign-out form may not have reached the server); it is also how someone
 * switches to another account. It cannot stop a person at that browser from sending the same request without it.
 *
 * Before the firewall, a GET of /oauth/authorize with `login` among its `prompt` values goes to the sign-in page
 * instead, which brings the browser back to the same request afterwards. A sign-in on the form marks that one request,
 * by a hash of its query, as answered; the request then goes on once, as whoever just signed in, and the mark is used
 * up. The mark lives only in the session, so no parameter anyone adds can skip the password.
 *
 * Nothing ends the browser's sign-in before someone signs in on the form: a link to such a request on another site
 * must not sign people out. The form's sign-in replaces it (Symfony renews the session id and cancels the old
 * remember-me cookie). Until then the browser stays signed in, so `prompt=login` is a request by the app, not a lock:
 * only the authorization request that carries it waits for the password.
 */
final readonly class PromptLogin
{
    public const string PARAMETER = 'prompt';
    public const string LOGIN = 'login';

    private const string PENDING = '_oauth_prompt_login.pending';
    private const string ANSWERED = '_oauth_prompt_login.answered';
    private const string TARGET_PATH = '_security.main.target_path';

    public function __construct(private UrlGeneratorInterface $urls)
    {
    }

    /** Above the firewall (8), below the router (32). */
    #[AsEventListener(event: KernelEvents::REQUEST, priority: 9)]
    public function onRequest(RequestEvent $event): void
    {
        $request = $event->getRequest();
        if (!$event->isMainRequest()
            || 'oauth2_authorize' !== $request->attributes->get('_route')
            || (!$request->isMethod('GET') && !$request->isMethod('HEAD'))
            || !self::asksToSignIn($request)
            || !$request->hasSession()
        ) {
            return;
        }

        $session = $request->getSession();
        $key = self::keyOf($request);
        if ($key === $session->get(self::ANSWERED)) {
            $session->remove(self::ANSWERED);

            return;
        }

        $session->set(self::PENDING, $key);
        $session->set(self::TARGET_PATH, $request->getUri());

        $response = new RedirectResponse($this->urls->generate('app_login', [], UrlGeneratorInterface::ABSOLUTE_URL));
        $response->headers->set('Cache-Control', 'no-store, private');
        $event->setResponse($response);
    }

    #[AsEventListener(event: LoginSuccessEvent::class, dispatcher: 'security.event_dispatcher.main')]
    public function onLogin(LoginSuccessEvent $event): void
    {
        $request = $event->getRequest();
        if (!$event->getAuthenticator() instanceof FormLoginAuthenticator || !$request->hasSession()) {
            return;
        }
        $session = $request->getSession();
        $pending = $session->get(self::PENDING);
        if (\is_string($pending)) {
            $session->remove(self::PENDING);
            $session->set(self::ANSWERED, $pending);
        }
    }

    private static function asksToSignIn(Request $request): bool
    {
        $prompt = $request->query->all()[self::PARAMETER] ?? null;

        return \is_string($prompt) && \in_array(self::LOGIN, explode(' ', $prompt), true);
    }

    /** The authorization request, as its normalized query (Symfony sorts the parameters). */
    private static function keyOf(Request $request): string
    {
        return hash('sha256', (string) $request->getQueryString());
    }
}
