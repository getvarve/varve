<?php

declare(strict_types=1);

namespace App\Controller;

use App\Security\ConfirmedEmailChecker;
use App\Security\PageHeaders;
use App\Security\SignOutGuard;
use Symfony\Bundle\FrameworkBundle\Controller\AbstractController;
use Symfony\Component\HttpFoundation\Response;
use Symfony\Component\Routing\Attribute\Route;
use Symfony\Component\Security\Core\Exception\CustomUserMessageAccountStatusException;
use Symfony\Component\Security\Http\Authentication\AuthenticationUtils;

/**
 * Browser sign-in used by the OAuth2 authorization endpoint. Clients never post credentials to the API directly.
 */
final class SecurityController extends AbstractController
{
    #[Route('/login', name: 'app_login', methods: ['GET', 'POST'])]
    public function login(AuthenticationUtils $authenticationUtils): Response
    {
        $error = $authenticationUtils->getLastAuthenticationError();

        return PageHeaders::apply($this->render('security/login.html.twig', [
            'last_username' => $authenticationUtils->getLastUsername(),
            'error' => $error,
            'unconfirmed' => $error instanceof CustomUserMessageAccountStatusException && ConfirmedEmailChecker::UNCONFIRMED === $error->getMessageKey(),
        ]));
    }

    /**
     * Never reached: {@see SignOutGuard} answers GET with a page and passes on only a POST from Varve, which the
     * firewall's logout listener handles.
     */
    #[Route(SignOutGuard::PATH, name: 'app_logout', methods: ['GET', 'HEAD', 'POST'])]
    public function logout(): never
    {
        throw new \LogicException('Intercepted by the logout key on the firewall.');
    }

    /** Where a sign-in ends that did not start in one of our apps; the sign-in page for a browser that is not signed in. */
    #[Route('/signed-in', name: 'app_signed_in', methods: ['GET'])]
    public function signedIn(): Response
    {
        if (null === $this->getUser()) {
            return PageHeaders::apply($this->redirectToRoute('app_login'));
        }

        return PageHeaders::apply($this->render('security/signed_in.html.twig'));
    }

    /** Where a sign-out ends while this server has no web client. */
    #[Route('/signed-out', name: 'app_signed_out', methods: ['GET'])]
    public function signedOut(): Response
    {
        return PageHeaders::apply($this->render('security/signed_out.html.twig'));
    }
}
