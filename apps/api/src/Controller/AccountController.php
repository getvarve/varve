<?php

declare(strict_types=1);

namespace App\Controller;

use App\Account\EmailConfirmation;
use App\Account\PasswordPolicy;
use App\Account\PasswordReset;
use App\Account\Registration;
use App\Account\TooManyAttempts;
use App\Entity\User;
use App\Security\PageHeaders;
use Psr\Log\LoggerInterface;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Mailer\Exception\TransportExceptionInterface;
use Symfony\Component\Routing\Attribute\Route;
use Symfony\Component\Validator\Constraints\Email;
use Symfony\Component\Validator\Validator\ValidatorInterface;

/**
 * The account pages in the browser, next to the sign-in page: signing up, confirming the address, asking for a new
 * confirmation link, and resetting a forgotten password. Every form posts back to its own page with a CSRF token.
 * What a page says after a post never tells whether an address has an account.
 */
final class AccountController extends AbstractController
{
    private const string CSRF_REGISTER = 'register';
    private const string CSRF_CONFIRM = 'confirm_email';
    private const string CSRF_RESEND = 'resend_confirmation';
    private const string CSRF_FORGOT = 'forgot_password';
    private const string CSRF_RESET = 'reset_password';

    private const string FORM_EXPIRED = 'This page was open too long or came from another site, so nothing happened. Try again.';
    private const string MAIL_FAILED = 'The mail could not be sent just now. Try again in a few minutes.';

    public function __construct(
        private readonly Registration $registration,
        private readonly EmailConfirmation $confirmation,
        private readonly PasswordReset $passwordReset,
        private readonly ValidatorInterface $validator,
        private readonly LoggerInterface $logger,
    ) {
    }

    #[Route('/register', name: 'app_register', methods: ['GET', 'POST'])]
    public function register(Request $request): Response
    {
        $values = ['email' => '', 'name' => ''];
        if (!$request->isMethod('POST')) {
            return $this->page('account/register.html.twig', ['values' => $values, 'errors' => []]);
        }

        $values = ['email' => trim($request->request->getString('email')), 'name' => trim($request->request->getString('name'))];
        $password = $request->request->getString('password');
        if (!$this->isCsrfTokenValid(self::CSRF_REGISTER, $request->request->getString('_csrf_token'))) {
            return $this->page('account/register.html.twig', ['values' => $values, 'errors' => ['form' => self::FORM_EXPIRED]], Response::HTTP_FORBIDDEN);
        }
        $errors = array_filter([
            'email' => $this->emailProblem($values['email']),
            'name' => self::nameProblem($values['name']),
            'password' => PasswordPolicy::problem($password),
        ]);
        if ([] !== $errors) {
            return $this->page('account/register.html.twig', ['values' => $values, 'errors' => $errors], Response::HTTP_UNPROCESSABLE_ENTITY);
        }

        try {
            $this->registration->register($values['email'], $values['name'], $password, $request->getClientIp());
        } catch (TooManyAttempts) {
            return $this->page('account/register.html.twig', ['values' => $values, 'errors' => ['form' => 'Too many sign-ups from your network. Try again in an hour.']], Response::HTTP_TOO_MANY_REQUESTS);
        } catch (TransportExceptionInterface $exception) {
            return $this->mailFailed('account/register.html.twig', ['values' => $values], $exception);
        }

        return $this->page('account/mail_sent.html.twig', ['email' => $values['email'], 'reason' => 'register']);
    }

    #[Route('/confirm-email', name: 'app_confirm_email', methods: ['GET', 'POST'])]
    public function confirmEmail(Request $request): Response
    {
        $token = $request->isMethod('POST') ? $request->request->getString('token') : $request->query->getString('token');
        if (!$request->isMethod('POST')) {
            return $this->page('account/confirm_email.html.twig', ['state' => $this->confirmation->isValid($token) ? 'ask' : 'invalid', 'token' => $token, 'error' => null], referrerPolicy: PageHeaders::NO_REFERRER);
        }

        if (!$this->isCsrfTokenValid(self::CSRF_CONFIRM, $request->request->getString('_csrf_token'))) {
            return $this->page('account/confirm_email.html.twig', ['state' => 'ask', 'token' => $token, 'error' => self::FORM_EXPIRED], Response::HTTP_FORBIDDEN, PageHeaders::NO_REFERRER);
        }
        if (!$this->confirmation->confirm($token)) {
            return $this->page('account/confirm_email.html.twig', ['state' => 'invalid', 'token' => '', 'error' => null], Response::HTTP_GONE, PageHeaders::NO_REFERRER);
        }

        return $this->page('account/confirm_email.html.twig', ['state' => 'confirmed', 'token' => '', 'error' => null], referrerPolicy: PageHeaders::NO_REFERRER);
    }

    #[Route('/confirm-email/resend', name: 'app_resend_confirmation', methods: ['GET', 'POST'])]
    public function resendConfirmation(Request $request): Response
    {
        if (!$request->isMethod('POST')) {
            return $this->page('account/resend_confirmation.html.twig', ['email' => $request->query->getString('email'), 'errors' => []]);
        }

        $email = trim($request->request->getString('email'));
        if (!$this->isCsrfTokenValid(self::CSRF_RESEND, $request->request->getString('_csrf_token'))) {
            return $this->page('account/resend_confirmation.html.twig', ['email' => $email, 'errors' => ['form' => self::FORM_EXPIRED]], Response::HTTP_FORBIDDEN);
        }
        $problem = $this->emailProblem($email);
        if (null !== $problem) {
            return $this->page('account/resend_confirmation.html.twig', ['email' => $email, 'errors' => ['email' => $problem]], Response::HTTP_UNPROCESSABLE_ENTITY);
        }

        try {
            $this->registration->resendConfirmation($email, $request->getClientIp());
        } catch (TransportExceptionInterface $exception) {
            return $this->mailFailed('account/resend_confirmation.html.twig', ['email' => $email], $exception);
        }

        return $this->page('account/mail_sent.html.twig', ['email' => $email, 'reason' => 'resend']);
    }

    #[Route('/forgot-password', name: 'app_forgot_password', methods: ['GET', 'POST'])]
    public function forgotPassword(Request $request): Response
    {
        if (!$request->isMethod('POST')) {
            return $this->page('account/forgot_password.html.twig', ['email' => $request->query->getString('email'), 'errors' => []]);
        }

        $email = trim($request->request->getString('email'));
        if (!$this->isCsrfTokenValid(self::CSRF_FORGOT, $request->request->getString('_csrf_token'))) {
            return $this->page('account/forgot_password.html.twig', ['email' => $email, 'errors' => ['form' => self::FORM_EXPIRED]], Response::HTTP_FORBIDDEN);
        }
        $problem = $this->emailProblem($email);
        if (null !== $problem) {
            return $this->page('account/forgot_password.html.twig', ['email' => $email, 'errors' => ['email' => $problem]], Response::HTTP_UNPROCESSABLE_ENTITY);
        }

        try {
            $this->passwordReset->request($email, $request->getClientIp());
        } catch (TransportExceptionInterface $exception) {
            return $this->mailFailed('account/forgot_password.html.twig', ['email' => $email], $exception);
        }

        return $this->page('account/mail_sent.html.twig', ['email' => $email, 'reason' => 'reset']);
    }

    #[Route('/reset-password', name: 'app_reset_password', methods: ['GET', 'POST'])]
    public function resetPassword(Request $request): Response
    {
        $token = $request->isMethod('POST') ? $request->request->getString('token') : $request->query->getString('token');
        if (!$request->isMethod('POST')) {
            return $this->page('account/reset_password.html.twig', ['state' => $this->passwordReset->isValid($token) ? 'form' : 'invalid', 'token' => $token, 'errors' => []], referrerPolicy: PageHeaders::NO_REFERRER);
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

    private function emailProblem(string $email): ?string
    {
        if ('' === $email || mb_strlen($email) > User::MAX_EMAIL || \count($this->validator->validate($email, new Email(mode: Email::VALIDATION_MODE_HTML5))) > 0) {
            return 'Enter a valid email address.';
        }

        return null;
    }

    private static function nameProblem(string $name): ?string
    {
        if ('' === $name || mb_strlen($name) > User::MAX_DISPLAY_NAME || 1 === preg_match('/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/u', $name) || !mb_check_encoding($name, 'UTF-8')) {
            return \sprintf('Enter your name, at most %d characters.', User::MAX_DISPLAY_NAME);
        }

        return null;
    }

    /** @param array<string, mixed> $context */
    private function mailFailed(string $template, array $context, TransportExceptionInterface $exception): Response
    {
        $this->logger->error('Account mail could not be sent: {message}', ['message' => $exception->getMessage(), 'exception' => $exception]);

        return $this->page($template, [...$context, 'errors' => ['form' => self::MAIL_FAILED]], Response::HTTP_SERVICE_UNAVAILABLE);
    }

    /** @param array<string, mixed> $context */
    private function page(string $template, array $context, int $status = Response::HTTP_OK, string $referrerPolicy = PageHeaders::SAME_ORIGIN): Response
    {
        return PageHeaders::apply($this->render($template, $context, new Response(status: $status)), $referrerPolicy);
    }
}
