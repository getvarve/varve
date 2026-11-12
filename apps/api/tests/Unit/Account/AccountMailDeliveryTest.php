<?php

declare(strict_types=1);

namespace App\Tests\Unit\Account;

use App\Account\AccountMailDelivery;
use PHPUnit\Framework\TestCase;
use Psr\Log\AbstractLogger;
use Symfony\Component\HttpFoundation\Request;
use Symfony\Component\HttpFoundation\RequestStack;
use Symfony\Component\Mailer\Envelope;
use Symfony\Component\Mailer\Exception\TransportException;
use Symfony\Component\Mailer\MailerInterface;
use Symfony\Component\Mime\Email;
use Symfony\Component\Mime\RawMessage;

final class AccountMailDeliveryTest extends TestCase
{
    public function testMailOfARequestLeavesAfterItsResponse(): void
    {
        $mailer = self::mailer();
        $delivery = new AccountMailDelivery($mailer, self::duringARequest(), self::logger());

        $delivery->send(self::mail('kim@example.test'));
        $delivery->send(self::mail('lisa@example.test'));
        self::assertSame([], $mailer->sent, 'Nothing while the response is not out.');

        $delivery->afterResponse();
        self::assertSame(['kim@example.test', 'lisa@example.test'], $mailer->sent);

        $delivery->afterResponse();
        self::assertCount(2, $mailer->sent, 'Each mail goes once.');
    }

    public function testAMailServerThatIsDownIsLoggedAndStopsNothing(): void
    {
        $mailer = self::mailer(failFor: 'kim@example.test');
        $logger = self::logger();
        $delivery = new AccountMailDelivery($mailer, self::duringARequest(), $logger);
        $delivery->send(self::mail('kim@example.test'));
        $delivery->send(self::mail('lisa@example.test'));

        $delivery->afterResponse();

        self::assertSame(['lisa@example.test'], $mailer->sent, 'The next mail still goes.');
        self::assertSame(['Account mail could not be sent: {message}'], $logger->errors);
    }

    public function testMailOfARequestThatNeverFinishedIsDropped(): void
    {
        $mailer = self::mailer();
        $delivery = new AccountMailDelivery($mailer, self::duringARequest(), self::logger());
        $delivery->send(self::mail('kim@example.test'));

        $delivery->reset();
        $delivery->afterResponse();

        self::assertSame([], $mailer->sent);
    }

    public function testOutsideARequestMailGoesAtOnce(): void
    {
        $mailer = self::mailer();
        $delivery = new AccountMailDelivery($mailer, new RequestStack(), self::logger());

        $delivery->send(self::mail('kim@example.test'));

        self::assertSame(['kim@example.test'], $mailer->sent);
    }

    private static function mail(string $to): Email
    {
        return new Email()->from('accounts@example.test')->to($to)->subject('Subject')->text('Text');
    }

    private static function duringARequest(): RequestStack
    {
        $requests = new RequestStack();
        $requests->push(Request::create('/forgot-password', 'POST'));

        return $requests;
    }

    /** @return MailerInterface&object{sent: list<string>} */
    private static function mailer(?string $failFor = null): MailerInterface
    {
        return new class($failFor) implements MailerInterface {
            /** @var list<string> */
            public array $sent = [];

            public function __construct(private readonly ?string $failFor)
            {
            }

            public function send(RawMessage $message, ?Envelope $envelope = null): void
            {
                TestCase::assertInstanceOf(Email::class, $message);
                $to = $message->getTo()[0]->getAddress();
                if ($to === $this->failFor) {
                    throw new TransportException('Connection refused.');
                }
                $this->sent[] = $to;
            }
        };
    }

    /** @return AbstractLogger&object{errors: list<string>} */
    private static function logger(): AbstractLogger
    {
        return new class extends AbstractLogger {
            /** @var list<string> */
            public array $errors = [];

            public function log($level, string|\Stringable $message, array $context = []): void
            {
                if ('error' === $level) {
                    $this->errors[] = (string) $message;
                }
            }
        };
    }
}
