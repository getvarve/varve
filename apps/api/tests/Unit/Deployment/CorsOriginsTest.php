<?php

declare(strict_types=1);

namespace App\Tests\Unit\Deployment;

use App\Deployment\CorsOrigins;
use PHPUnit\Framework\TestCase;

final class CorsOriginsTest extends TestCase
{
    public function testAListAllowsExactlyThoseOrigins(): void
    {
        $pattern = '{'.CorsOrigins::pattern(' https://varve.example.com  tauri://localhost http://tauri.localhost ').'}i';

        foreach (['https://varve.example.com', 'tauri://localhost', 'http://tauri.localhost'] as $allowed) {
            self::assertSame(1, preg_match($pattern, $allowed), $allowed);
        }
        foreach (["https://varve.example.com\n", 'https://mailxexample.com', 'https://varve.example.com.evil.com', 'https://evil.varve.example.com', 'http://tauri.localhost.evil.com', 'https://varve.example.com:8443'] as $refused) {
            self::assertSame(0, preg_match($pattern, $refused), $refused);
        }
    }

    public function testAPatternStaysAsItIs(): void
    {
        self::assertSame('^(https?://localhost(:[0-9]+)?)$', CorsOrigins::pattern('^(https?://localhost(:[0-9]+)?)$'));
    }

    public function testAnEmptyListAllowsNoOrigin(): void
    {
        $pattern = '{'.CorsOrigins::pattern('  ').'}i';

        self::assertSame(0, preg_match($pattern, 'https://varve.example.com'));
        self::assertSame(0, preg_match($pattern, ''));
    }
}
