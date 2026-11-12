<?php

declare(strict_types=1);

namespace App\Device;

use App\Entity\Device;
use App\Enum\DevicePlatform;
use Symfony\Component\HttpFoundation\Request;

/**
 * How an app names the device it signs in on, in its token request (form encoded, next to the OAuth2 parameters):
 * `device_name` ("MacBook Pro", at most 100 characters), `device_platform` (`macos`, `windows`, `linux` or `web`) and
 * `app_version` (such as `0.4.0`). The code exchange needs all three; a refresh may send `app_version` again after
 * the app was updated.
 */
final readonly class DeviceDescription
{
    public const string NAME = 'device_name';
    public const string PLATFORM = 'device_platform';
    public const string APP_VERSION = 'app_version';

    private function __construct(
        public string $name,
        public DevicePlatform $platform,
        public string $appVersion,
    ) {
    }

    /**
     * @return self|string the description, or what is wrong with it
     */
    public static function fromRequest(Request $request): self|string
    {
        $name = self::name($request->request->get(self::NAME));
        if (null === $name) {
            return \sprintf('Name the device in "%s": 1 to %d characters, no control characters.', self::NAME, Device::MAX_NAME);
        }
        $platform = $request->request->get(self::PLATFORM);
        $platform = \is_string($platform) ? DevicePlatform::tryFrom($platform) : null;
        if (null === $platform) {
            return \sprintf('Send "%s" as one of: %s.', self::PLATFORM, implode(', ', array_map(static fn (DevicePlatform $platform): string => $platform->value, DevicePlatform::cases())));
        }
        $version = self::appVersion($request->request->get(self::APP_VERSION));
        if (null === $version) {
            return self::appVersionProblem();
        }

        return new self($name, $platform, $version);
    }

    /** A device name as the user or the app gives it, trimmed; null when it is empty, too long or has control characters. */
    public static function name(mixed $value): ?string
    {
        if (!\is_string($value) || !mb_check_encoding($value, 'UTF-8')) {
            return null;
        }
        $name = trim($value);
        if ('' === $name || mb_strlen($name) > Device::MAX_NAME || 1 === preg_match('/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/u', $name)) {
            return null;
        }

        return $name;
    }

    /** An app version: letters, digits, dots, plus and minus, starting with a letter or digit; null otherwise. */
    public static function appVersion(mixed $value): ?string
    {
        if (!\is_string($value) || 1 !== preg_match('/^[0-9A-Za-z][0-9A-Za-z.+-]*$/', $value) || \strlen($value) > Device::MAX_APP_VERSION) {
            return null;
        }

        return $value;
    }

    public static function appVersionProblem(): string
    {
        return \sprintf('Send "%s" as the app\'s version, such as 0.4.0: at most %d letters, digits, dots, plus and minus signs.', self::APP_VERSION, Device::MAX_APP_VERSION);
    }
}
