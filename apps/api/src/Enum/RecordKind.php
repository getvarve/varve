<?php

declare(strict_types=1);

namespace App\Enum;

/**
 * The kinds of synced records an organization holds (design 8.6): what the desktop app keeps in its folder tree.
 * Job profiles and workspace settings join when the app has them.
 */
enum RecordKind: string
{
    case Folder = 'folder';
    case Connection = 'connection';
    case Query = 'query';

    /** Whether every accepted change keeps the payload it replaces as a revision (design 8.11). */
    public function keepsRevisions(): bool
    {
        return self::Query === $this;
    }
}
