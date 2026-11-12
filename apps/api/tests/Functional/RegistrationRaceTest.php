<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Entity\User;
use DAMA\DoctrineTestBundle\PHPUnit\PHPUnitExtension;
use Doctrine\DBAL\Connection;
use Doctrine\DBAL\DriverManager;
use Doctrine\ORM\EntityManagerInterface;
use Doctrine\ORM\Event\OnFlushEventArgs;
use Doctrine\ORM\Events;
use Symfony\Component\Uid\Uuid;

/**
 * Two sign-ups with one new address at the same moment. A second connection, outside the test's transaction, plays
 * the sign-up that wins: it stores the account after this sign-up looked the address up and before this sign-up
 * stores its own. What it stored is committed, so it is deleted after the class, once the test's own transaction is
 * rolled back (which holds a lock on that row until then), and before the test, for a run that was cut short.
 *
 * @phpstan-import-type Params from DriverManager
 */
final class RegistrationRaceTest extends DatabaseTestCase
{
    use AuthenticatesUsers;
    use ReadsAccountMail;

    private const string EMAIL = 'raced@example.test';
    private const string PASSWORD = 'a-long-enough-password';

    private static ?Connection $winner = null;

    public static function tearDownAfterClass(): void
    {
        // The extension rolls a test back only when the next one starts; this cannot wait for that.
        PHPUnitExtension::rollBack();
        self::$winner?->executeStatement('DELETE FROM app_user WHERE email = :email', ['email' => self::EMAIL]);
        self::$winner?->close();
        self::$winner = null;
        parent::tearDownAfterClass();
    }

    protected function setUp(): void
    {
        parent::setUp();
        /** @var Params $parameters */
        $parameters = $this->connection()->getParams();
        self::$winner = DriverManager::getConnection($parameters);
        self::$winner->executeStatement('DELETE FROM app_user WHERE email = :email', ['email' => self::EMAIL]);
        $this->bootstrapOAuthClients();
        $this->resetRateLimits();
    }

    public function testBothGetTheSamePageAndTheLaterOneGetsALinkForTheAccountOfTheFirst(): void
    {
        self::assertNotNull(self::$winner);
        $race = new class(self::$winner, self::EMAIL) {
            public bool $ran = false;

            public function __construct(private readonly Connection $connection, private readonly string $email)
            {
            }

            public function onFlush(OnFlushEventArgs $event): void
            {
                foreach ($event->getObjectManager()->getUnitOfWork()->getScheduledEntityInsertions() as $entity) {
                    if ($entity instanceof User && $entity->getEmail() === $this->email && !$this->ran) {
                        $this->ran = true;
                        $this->connection->executeStatement(
                            "INSERT INTO app_user (id, email, password, display_name, roles, created_at) VALUES (:id, :email, '', 'Someone Else', '[]', NOW())",
                            ['id' => Uuid::v7()->toRfc4122(), 'email' => $this->email],
                        );
                    }
                }
            }
        };
        $events = static::getContainer()->get(EntityManagerInterface::class)->getEventManager();
        $events->addEventListener(Events::onFlush, $race);

        try {
            $crawler = $this->client->request('GET', '/register');
            $this->client->submit($crawler->selectButton('Create account')->form(['email' => self::EMAIL, 'name' => 'Noor', 'password' => self::PASSWORD]));
        } finally {
            $events->removeEventListener(Events::onFlush, $race);
        }

        self::assertTrue($race->ran);
        self::assertResponseIsSuccessful();
        self::assertSelectorTextContains('h1', 'Check your mail');
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM app_user WHERE email = :email', ['email' => self::EMAIL]));
        $secret = $this->linkSecret($this->sentMail(self::EMAIL), '/confirm-email');
        $crawler = $this->client->request('GET', '/confirm-email?token='.$secret);
        $this->client->submit($crawler->selectButton('Confirm')->form(['password' => self::PASSWORD]));
        self::assertSelectorTextContains('h1', 'Address confirmed', 'This sign-up got a link for the account the other one made.');
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM app_user WHERE email = :email AND display_name = 'Noor' AND email_confirmed_at IS NOT NULL", ['email' => self::EMAIL]));
    }
}
