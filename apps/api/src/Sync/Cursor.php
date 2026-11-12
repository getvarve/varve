<?php

declare(strict_types=1);

namespace App\Sync;

/**
 * Where a client is in a change feed. Opaque to clients: base64url of "1:<scope tag>:<seq>:<full>".
 *
 * `seq` is the last change number the client has. `full` is set while a first (full) pull pages through the records:
 * the change number the feed was at when it started. Tombstones up to it are left out, since the client never had
 * those records; anything deleted later is still reported.
 */
final readonly class Cursor
{
    private const string VERSION = '1';

    public function __construct(
        public int $seq,
        public int $full = 0,
    ) {
    }

    public function encode(SyncScope $scope): string
    {
        $full = $this->seq < $this->full ? $this->full : 0;

        return rtrim(strtr(base64_encode(implode(':', [self::VERSION, $scope->cursorTag(), $this->seq, $full])), '+/', '-_'), '=');
    }

    /** The cursor this text encodes for this scope; null when it is not one, or one of another scope. */
    public static function decode(string $text, SyncScope $scope): ?self
    {
        if (1 !== preg_match('/^[A-Za-z0-9_-]{1,200}$/', $text)) {
            return null;
        }
        $decoded = base64_decode(strtr($text, '-_', '+/'), true);
        if (false === $decoded || 1 !== preg_match('/^(\d+):([ou][0-9a-f-]{36}):(\d{1,18}):(\d{1,18})$/', $decoded, $parts)) {
            return null;
        }
        if (self::VERSION !== $parts[1] || $scope->cursorTag() !== $parts[2]) {
            return null;
        }

        return new self((int) $parts[3], (int) $parts[4]);
    }

    /** The change number up to which the client has everything that matters: what a purge must not reach past. */
    public function horizon(): int
    {
        return max($this->seq, $this->full);
    }
}
