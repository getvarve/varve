<?php

declare(strict_types=1);

namespace App\Security;

use Symfony\Component\Security\Core\Authentication\Token\TokenInterface;
use Symfony\Component\Security\Core\Authorization\Voter\Vote;
use Symfony\Component\Security\Core\Authorization\Voter\Voter;

/**
 * Grants {@see FirstPartyClients::ATTRIBUTE} to requests signed by one of our own apps.
 *
 * @extends Voter<string, mixed>
 */
final class FirstPartyClientVoter extends Voter
{
    protected function supports(string $attribute, mixed $subject): bool
    {
        return FirstPartyClients::ATTRIBUTE === $attribute;
    }

    protected function voteOnAttribute(string $attribute, mixed $subject, TokenInterface $token, ?Vote $vote = null): bool
    {
        return FirstPartyClients::signedRequest($token);
    }
}
