<?php

declare(strict_types=1);

namespace App\Controller;

use App\Account\AccountDetails;
use App\Account\ConfirmationResult;
use App\Account\EmailConfirmation;
use App\Account\PasswordPolicy;
use App\Account\PasswordReset;
use App\Account\Registration;
use App\Account\SignUpSettings;
use App\Account\TooManyAttempts;
use App\Entity\User;
use App\Security\PageHeaders;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Routing\Attribute\Route;

/**
 * The account pages in the browser, next to the sign-in page: signing up, confirming the address, asking for a new
 * confirmation link, and resetting a forgotten password. Every form posts back to its own page with a CSRF token.
 * What a page says after a post never tells whether an address has an account, and no mail is sent before the page
 * is: a mail server that is slow or down changes nothing a visitor sees.
 *
 * A post that comes without the session cookie (as a form on another site sends it) gets a 303 to the page itself:
 * no token can match without the session, and rendering the page would start a new session whose cookie replaces the
 * browser's, which would sign that browser out.
 *
 * While sign-up is closed ({@see SignUpSettings}), the sign-up page, the confirmation page and the page that sends a
 * confirmation link again only say so.
 */
final class AccountController extends AbstractController
{
    private const string CSRF_REGISTER = 'register';
    private const string CSRF_CONFIRM = 'confirm_email';
    private const string CSRF_RESEND = 'resend_confirmation';
    private const string CSRF_FORGOT = 'forgot_password';
    private const string CSRF_RESET = 'reset_password';

    private const string FORM_EXPIRED = 'This page was open too long or came from another site, so nothing happened. Try again.';

    public function __construct(
        private readonly Registration $registration,
        private readonly EmailConfirmation $confirmation,
        private readonly PasswordReset $passwordReset,
        private readonly AccountDetails $details,
        private readonly SignUpSettings $signUp,
    ) {
    }

    #[Route('/register', name: 'app_register', methods: ['GET', 'POST'])]
    public function register(Request $request): Response
    {
        if (!$this->signUp->open) {
            return $this->signUpClosed();
        }
        $values = ['email' => '', 'name' => ''];
        if (!$request->isMethod('POST')) {
            return $this->page('account/register.html.twig', ['values' => $values, 'errors' => []]);
        }
        if (!$request->hasPreviousSession()) {
            return $this->backToThePage($request);
        }

        $values = ['email' => trim($request->request->getString('email')), 'name' => trim($request->request->getString('name'))];
        $password = $request->request->getString('password');
        if (!$this->isCsrfTokenValid(self::CSRF_REGISTER, $request->request->getString('_csrf_token'))) {
            return $this->page('account/register.html.twig', ['values' => $values, 'errors' => ['form' => self::FORM_EXPIRED]], Response::HTTP_FORBIDDEN);
        }
        $errors = array_filter([
            'email' => $this->details->emailProblem($values['email']),
            'name' => AccountDetails::nameProblem($values['name']),
            'password' => PasswordPolicy::problem($password),
        ]);
        if ([] !== $errors) {
            return $this->page('account/register.html.twig', ['values' => $values, 'errors' => $errors], Response::HTTP_UNPROCESSABLE_ENTITY);
        }

        try {
            $this->registration->register($values['email'], $values['name'], $password, $request->getClientIp());
        } catch (TooManyAttempts) {
            return $this->page('account/register.html.twig', ['values' => $values, 'errors' => ['form' => 'Too many sign-ups from your network. Try again in an hour.']], Response::HTTP_TOO_MANY_REQUESTS);
        }

        return $this->page('account/mail_sent.html.twig', ['email' => $values['email'], 'reason' => 'register']);
    }

    #[Route('/confirm-email', name: 'app_confirm_email', methods: ['GET', 'POST'])]
    public function confirmEmail(Request $request): Response
    {
        if (!$this->signUp->open) {
            return $this->signUpClosed();
        }
        $token = $request->isMethod('POST') ? $request->request->getString('token') : $request->query->getString('token');
        if (!$request->isMethod('POST')) {
            $state = match (true) {
                $this->confirmation->isValid($token) => 'ask',
                $this->confirmation->isExpired($token) => 'expired',
                default => 'invalid',
            };

            return $this->page('account/confirm_email.html.twig', ['state' => $state, 'token' => $token, 'errors' => []], referrerPolicy: PageHeaders::NO_REFERRER);
        }
        if (!$request->hasPreviousSession()) {
            return $this->backToThePage($request, $token);
        }

        if (!$this->isCsrfTokenValid(self::CSRF_CONFIRM, $request->request->getString('_csrf_token'))) {
            return $this->page('account/confirm_email.html.twig', ['state' => 'ask', 'token' => $token, 'errors' => ['form' => self::FORM_EXPIRED]], Response::HTTP_FORBIDDEN, PageHeaders::NO_REFERRER);
        }

        return match ($this->confirmation->confirm($token, $request->request->getString('password'))) {
            ConfirmationResult::Confirmed => $this->page('account/confirm_email.html.twig', ['state' => 'confirmed', 'token' => '', 'errors' => []], referrerPolicy: PageHeaders::NO_REFERRER),
            ConfirmationResult::WrongPassword => $this->page('account/confirm_email.html.twig', ['state' => 'ask', 'token' => $token, 'errors' => ['password' => 'This is not the password you chose when you signed up.']], Response::HTTP_UNPROCESSABLE_ENTITY, PageHeaders::NO_REFERRER),
            ConfirmationResult::LinkEnded => $this->page('account/confirm_email.html.twig', ['state' => 'invalid', 'token' => '', 'errors' => []], Response::HTTP_GONE, PageHeaders::NO_REFERRER),
        };
    }

    #[Route('/confirm-email/resend', name: 'app_resend_confirmation', methods: ['GET', 'POST'])]
    public function resendConfirmation(Request $request): Response
    {
        if (!$this->signUp->open) {
            return $this->signUpClosed();
        }
        if (!$request->isMethod('POST')) {
            return $this->page('account/resend_confirmation.html.twig', ['email' => $request->query->getString('email'), 'errors' => []]);
        }
        if (!$request->hasPreviousSession()) {
            return $this->backToThePage($request);
        }

        $email = trim($request->request->getString('email'));
        if (!$this->isCsrfTokenValid(self::CSRF_RESEND, $request->request->getString('_csrf_token'))) {
            return $this->page('account/resend_confirmation.html.twig', ['email' => $email, 'errors' => ['form' => self::FORM_EXPIRED]], Response::HTTP_FORBIDDEN);
        }
        // From the page of an expired link: its holder asks for a new one to the same address.
        $link = $request->request->getString('link');
        if ('' !== $link) {
            $this->registration->resendConfirmationWithLink($link);

            return $this->page('account/mail_sent.html.twig', ['email' => '', 'reason' => 'resend']);
        }
        $problem = $this->details->emailProblem($email);
        if (null !== $problem) {
            return $this->page('account/resend_confirmation.html.twig', ['email' => $email, 'errors' => ['email' => $problem]], Response::HTTP_UNPROCESSABLE_ENTITY);
        }

        $this->registration->resendConfirmation($email, $request->getClientIp());

        return $this->page('account/mail_sent.html.twig', ['email' => $email, 'reason' => 'resend']);
    }

    #[Route('/forgot-password', name: 'app_forgot_password', methods: ['GET', 'POST'])]
    public function forgotPassword(Request $request): Response
    {
        if (!$request->isMethod('POST')) {
            return $this->page('account/forgot_password.html.twig', ['email' => $request->query->getString('email'), 'errors' => []]);
        }
        if (!$request->hasPreviousSession()) {
            return $this->backToThePage($request);
        }

        $email = trim($request->request->getString('email'));
        if (!$this->isCsrfTokenValid(self::CSRF_FORGOT, $request->request->getString('_csrf_token'))) {
            return $this->page('account/forgot_password.html.twig', ['email' => $email, 'errors' => ['form' => self::FORM_EXPIRED]], Response::HTTP_FORBIDDEN);
        }
        // From the page of an expired link: its holder asks for a new one to the same address.
        $link = $request->request->getString('link');
        if ('' !== $link) {
            $this->passwordReset->requestWithLink($link);

            return $this->page('account/mail_sent.html.twig', ['email' => '', 'reason' => 'reset']);
        }
        $problem = $this->details->emailProblem($email);
        if (null !== $problem) {
            return $this->page('account/forgot_password.html.twig', ['email' => $email, 'errors' => ['email' => $problem]], Response::HTTP_UNPROCESSABLE_ENTITY);
        }

        $signedIn = $this->getUser();
        $this->passwordReset->request($email, $request->getClientIp(), $signedIn instanceof User ? $signedIn : null);

        return $this->page('account/mail_sent.html.twig', ['email' => $email, 'reason' => 'reset']);
    }

    #[Route('/reset-password', name: 'app_reset_password', methods: ['GET', 'POST'])]
    public function resetPassword(Request $request): Response
    {
        $token = $request->isMethod('POST') ? $request->request->getString('token') : $request->query->getString('token');
        if (!$request->isMethod('POST')) {
            $state = match (true) {
                $this->passwordReset->isValid($token) => 'form',
                $this->passwordReset->isExpired($token) => 'expired',
                default => 'invalid',
            };

            return $this->page('account/reset_password.html.twig', ['state' => $state, 'token' => $token, 'errors' => []], referrerPolicy: PageHeaders::NO_REFERRER);
        }
        if (!$request->hasPreviousSession()) {
            return $this->backToThePage($request, $token);
        }

        if (!$this->isCsrfTokenValid(self::CSRF_RESET, $request->request->getString('_csrf_token'))) {
            return $this->page('account/reset_password.html.twig', ['state' => 'form', 'token' => $token, 'errors' => ['form' => self::FORM_EXPIRED]], Response::HTTP_FORBIDDEN, PageHeaders::NO_REFERRER);
        }
        $password = $request->request->getString('password');
        $problem = PasswordPolicy::problem($password);
        if (null !== $problem) {
            return $this->page('account/reset_password.html.twig', ['state' => 'form', 'token' => $token, 'errors' => ['password' => $problem]], Response::HTTP_UNPROCESSABLE_ENTITY, PageHeaders::NO_REFERRER);
        }
        if (!$this->passwordReset->reset($token, $password)) {
            return $this->page('account/reset_password.html.twig', ['state' => 'invalid', 'token' => '', 'errors' => []], Response::HTTP_GONE, PageHeaders::NO_REFERRER);
        }

        return $this->page('account/reset_password.html.twig', ['state' => 'done', 'token' => '', 'errors' => []], referrerPolicy: PageHeaders::NO_REFERRER);
    }

    private function signUpClosed(): Response
    {
        return $this->page('account/sign_up_closed.html.twig', [], Response::HTTP_FORBIDDEN);
    }

    /**
     * Where a post without the session cookie goes: the page it was meant for, as a navigation that carries the
     * browser's cookies. The page of a mail link is opened with the secret the post named, when it looks like one.
     */
    private function backToThePage(Request $request, #[\SensitiveParameter] string $token = ''): Response
    {
        $query = 1 === preg_match('/^[0-9a-f]{64}$/', $token) ? '?'.http_build_query(['token' => $token]) : '';

        return PageHeaders::apply($this->redirect($request->getBaseUrl().$request->getPathInfo().$query, Response::HTTP_SEE_OTHER), '' === $query ? PageHeaders::SAME_ORIGIN : PageHeaders::NO_REFERRER);
    }

    /** @param array<string, mixed> $context */
    private function page(string $template, array $context, int $status = Response::HTTP_OK, string $referrerPolicy = PageHeaders::SAME_ORIGIN): Response
    {
        return PageHeaders::apply($this->render($template, $context, new Response(status: $status)), $referrerPolicy);
    }
}
