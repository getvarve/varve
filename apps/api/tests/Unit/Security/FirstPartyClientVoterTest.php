<?php

declare(strict_types=1);

namespace App\Tests\Unit\Security;

use App\Security\FirstPartyClients;
use App\Security\FirstPartyClientVoter;
use League\Bundle\OAuth2ServerBundle\Security\Authentication\Token\OAuth2Token;
use PHPUnit\Framework\TestCase;
use Symfony\Component\Security\Core\Authentication\Token\UsernamePasswordToken;
use Symfony\Component\Security\Core\Authorization\Voter\VoterInterface;
use Symfony\Component\Security\Core\User\InMemoryUser;

final class FirstPartyClientVoterTest extends TestCase
{
    public function testOnlyRequestsSignedByOurOwnAppsAreGranted(): void
    {
        $voter = new FirstPartyClientVoter();
        $user = new InMemoryUser('kim@example.test', null);

        foreach (['desktop' => VoterInterface::ACCESS_GRANTED, 'web' => VoterInterface::ACCESS_GRANTED, 'someone-elses-app' => VoterInterface::ACCESS_DENIED] as $client => $expected) {
            $token = new OAuth2Token($user, 'token-id', $client, ['account'], 'ROLE_OAUTH2_');

            self::assertSame($expected, $voter->vote($token, null, [FirstPartyClients::ATTRIBUTE]), $client);
        }
    }

    public function testABrowserSessionIsNotAnApp(): void
    {
        $token = new UsernamePasswordToken(new InMemoryUser('kim@example.test', null), 'main', ['ROLE_USER']);

        self::assertSame(VoterInterface::ACCESS_DENIED, new FirstPartyClientVoter()->vote($token, null, [FirstPartyClients::ATTRIBUTE]));
    }

    public function testOtherAttributesAreNotItsBusiness(): void
    {
        $token = new OAuth2Token(new InMemoryUser('kim@example.test', null), 'token-id', 'desktop', ['account'], 'ROLE_OAUTH2_');

        self::assertSame(VoterInterface::ACCESS_ABSTAIN, new FirstPartyClientVoter()->vote($token, null, ['ROLE_USER']));
    }
}
