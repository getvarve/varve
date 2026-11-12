<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use Symfony\Component\Mime\Email;

/**
 * The account mail the last request sent, and the links in it.
 */
trait ReadsAccountMail
{
    /** The one mail the last request sent, to this address. */
    protected function sentMail(string $to): Email
    {
        self::assertEmailCount(1);
        $email = self::getMailerMessage();
        self::assertInstanceOf(Email::class, $email);
        self::assertSame($to, $email->getTo()[0]->getAddress());
        self::assertSame('accounts@example.test', $email->getFrom()[0]->getAddress());
        self::assertSame('auto-generated', $email->getHeaders()->getHeaderBody('Auto-Submitted'));

        return $email;
    }

    /** The secret of the link to this page in a mail, after checking that the link is on the server's address. */
    protected function linkSecret(Email $email, string $path): string
    {
        $text = (string) $email->getTextBody();
        if (1 !== preg_match('{'.preg_quote($this->apiAddress().$path, '{').'\?token=([0-9a-f]{64})}', $text, $match)) {
            self::fail('No link to '.$path.' in: '.$text);
        }
        self::assertStringContainsString($match[0], (string) $email->getHtmlBody(), 'The HTML part has the same link.');

        return $match[1];
    }
}
