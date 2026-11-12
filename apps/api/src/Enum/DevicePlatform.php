<?php

declare(strict_types=1);

namespace App\Enum;

/**
 * What a device runs on: the desktop app on one of its systems, or the web client in a browser.
 */
enum DevicePlatform: string
{
    case MacOs = 'macos';
    case Windows = 'windows';
    case Linux = 'linux';
    case Web = 'web';
}
