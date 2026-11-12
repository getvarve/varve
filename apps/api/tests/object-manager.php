<?php

declare(strict_types=1);

use App\Kernel;
use Symfony\Component\Dotenv\Dotenv;

require dirname(__DIR__).'/vendor/autoload.php';

new Dotenv()->bootEnv(dirname(__DIR__).'/.env');

$env = $_SERVER['APP_ENV'] ?? 'dev';
\assert(\is_string($env));
$kernel = new Kernel($env, (bool) ($_SERVER['APP_DEBUG'] ?? true));
$kernel->boot();

return $kernel->getContainer()->get('doctrine')->getManager();
