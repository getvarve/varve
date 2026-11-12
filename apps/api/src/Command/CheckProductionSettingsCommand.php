<?php

declare(strict_types=1);

namespace App\Command;

use App\Deployment\ProductionSettings;
use Symfony\Component\Console\Attribute\AsCommand;
use Symfony\Component\Console\Command\Command;
use Symfony\Component\Console\Style\SymfonyStyle;
use Symfony\Component\DependencyInjection\Attribute\Autowire;
use Symfony\Component\Dotenv\Dotenv;

/**
 * Refuses a production start whose settings are missing or still development values. The production image runs it
 * before anything else, so a forgotten secret stops the server instead of falling back to a public one.
 */
#[AsCommand(name: 'app:settings:check', description: 'Check that the production settings are complete and not development values')]
final readonly class CheckProductionSettingsCommand
{
    public function __construct(
        #[Autowire('%kernel.project_dir%')]
        private string $projectDir,
    ) {
    }

    public function __invoke(SymfonyStyle $io): int
    {
        $settings = new ProductionSettings($this->environment(), $this->developmentValues());
        $problems = $settings->problems();
        if ([] !== $problems) {
            $io->error(['The production settings are not safe to start with:', ...$problems]);

            return Command::FAILURE;
        }

        $io->success('The production settings are complete.');

        return Command::SUCCESS;
    }

    /** @return array<string, string> */
    private function environment(): array
    {
        $values = [];
        foreach ([...$_ENV, ...$_SERVER] as $name => $value) {
            if (\is_string($name) && \is_string($value)) {
                $values[$name] = $value;
            }
        }

        return $values;
    }

    /**
     * The values the repository's .env files hold. A setting may match any of them: they are all public.
     *
     * @return array<string, list<string>>
     */
    private function developmentValues(): array
    {
        $values = [];
        foreach (['.env', '.env.dev', '.env.test'] as $file) {
            $path = $this->projectDir.'/'.$file;
            if (!is_file($path)) {
                continue;
            }
            /** @var array<string, string> $parsed */
            $parsed = new Dotenv()->parse((string) file_get_contents($path), $path);
            foreach ($parsed as $name => $value) {
                $values[$name][] = $value;
            }
        }

        return $values;
    }
}
