<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Command\BootstrapOAuthClientsCommand;
use App\Deployment\PublicAddresses;
use League\Bundle\OAuth2ServerBundle\Manager\ClientManagerInterface;
use League\Bundle\OAuth2ServerBundle\Model\AbstractClient;
use Symfony\Component\Console\Input\ArrayInput;
use Symfony\Component\Console\Output\BufferedOutput;
use Symfony\Component\Console\Style\SymfonyStyle;

final class BootstrapOAuthClientsCommandTest extends DatabaseTestCase
{
    use AuthenticatesUsers;

    public function testMakesOurTwoPublicClients(): void
    {
        $this->bootstrapOAuthClients();
        $this->bootstrapOAuthClients();

        $clients = static::getContainer()->get(ClientManagerInterface::class);
        $desktop = $clients->find('desktop');
        self::assertInstanceOf(AbstractClient::class, $desktop);
        self::assertTrue($desktop->isActive());
        self::assertNull($desktop->getSecret(), 'A public client.');
        self::assertFalse($desktop->isPlainTextPkceAllowed());
        self::assertSame(['varve://auth/callback', 'http://127.0.0.1/auth/callback', 'http://[::1]/auth/callback'], array_map(strval(...), $desktop->getRedirectUris()));
        self::assertSame(['authorization_code', 'refresh_token'], array_map(strval(...), $desktop->getGrants()));
        self::assertSame(['account', 'sync'], array_map(strval(...), $desktop->getScopes()));

        $web = $clients->find('web');
        self::assertInstanceOf(AbstractClient::class, $web);
        self::assertTrue($web->isActive());
        self::assertSame(['http://localhost:5173/auth/callback'], array_map(strval(...), $web->getRedirectUris()));
        self::assertSame(2, $this->countRows('SELECT COUNT(*) FROM oauth2_client'), 'Running it again changes nothing.');
    }

    public function testWithoutAWebClientAddressTheWebClientCannotSignIn(): void
    {
        $this->bootstrapOAuthClients();
        $clients = static::getContainer()->get(ClientManagerInterface::class);
        $output = new BufferedOutput();

        $status = new BootstrapOAuthClientsCommand($clients, new PublicAddresses('http://localhost', ''))(new SymfonyStyle(new ArrayInput([]), $output));

        self::assertSame(0, $status);
        self::assertStringContainsString('The web client is off', $output->fetch());
        $web = $clients->find('web');
        self::assertInstanceOf(AbstractClient::class, $web);
        self::assertFalse($web->isActive());
        self::assertSame([], $web->getRedirectUris());
        $desktop = $clients->find('desktop');
        self::assertInstanceOf(AbstractClient::class, $desktop);
        self::assertTrue($desktop->isActive());
    }
}
