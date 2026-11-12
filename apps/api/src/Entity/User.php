<?php

declare(strict_types=1);

namespace App\Entity;

use App\Repository\UserRepository;
use Doctrine\DBAL\Types\Types;
use Doctrine\ORM\Mapping as ORM;
use Symfony\Bridge\Doctrine\Types\UuidType;
use Symfony\Component\Security\Core\User\PasswordAuthenticatedUserInterface;
use Symfony\Component\Security\Core\User\UserInterface;
use Symfony\Component\Uid\Uuid;

/**
 * An account: an email address and a password. It can sign in once the address is confirmed (the link in the
 * confirmation mail, or a password reset, which proves the same). Every user owns a personal organization from the
 * moment the account exists ({@see Organization}).
 */
#[ORM\Entity(repositoryClass: UserRepository::class)]
#[ORM\Table(name: 'app_user')]
#[ORM\Index(columns: ['email_confirmed_at', 'created_at'])]
class User implements UserInterface, PasswordAuthenticatedUserInterface
{
    public const int MAX_EMAIL = 320;
    public const int MAX_DISPLAY_NAME = 120;

    #[ORM\Id]
    #[ORM\Column(type: UuidType::NAME)]
    private Uuid $id;

    #[ORM\Column(length: self::MAX_EMAIL, unique: true)]
    private string $email;

    #[ORM\Column(length: 255)]
    private string $password = '';

    #[ORM\Column(length: self::MAX_DISPLAY_NAME)]
    private string $displayName;

    /** @var list<string> */
    #[ORM\Column(type: Types::JSON)]
    private array $roles = [];

    /** When the user proved the address is theirs; null until then, and no sign-in before it. */
    #[ORM\Column(nullable: true)]
    private ?\DateTimeImmutable $emailConfirmedAt = null;

    #[ORM\Column]
    private \DateTimeImmutable $createdAt;

    /**
     * The last change number of the user's own records ({@see UserRecord}), as {@see Organization::$recordSeq} is for
     * an organization's. Written by App\Sync in SQL.
     */
    #[ORM\Column(type: Types::BIGINT, options: ['default' => 0])]
    private int $recordSeq = 0;

    /** The highest change number of a purged tombstone of the user's own records. */
    #[ORM\Column(type: Types::BIGINT, options: ['default' => 0])]
    private int $recordPurgeSeq = 0;

    public function __construct(string $email, string $displayName, \DateTimeImmutable $createdAt)
    {
        $this->id = Uuid::v7();
        $this->email = mb_strtolower($email);
        $this->displayName = $displayName;
        $this->createdAt = $createdAt;
    }

    public function getId(): Uuid
    {
        return $this->id;
    }

    public function getEmail(): string
    {
        return $this->email;
    }

    public function getDisplayName(): string
    {
        return $this->displayName;
    }

    public function setDisplayName(string $displayName): void
    {
        $this->displayName = $displayName;
    }

    public function getUserIdentifier(): string
    {
        \assert('' !== $this->email);

        return $this->email;
    }

    public function getRoles(): array
    {
        return array_values(array_unique([...$this->roles, 'ROLE_USER']));
    }

    /** @param list<string> $roles */
    public function setRoles(array $roles): void
    {
        $this->roles = $roles;
    }

    public function getPassword(): string
    {
        return $this->password;
    }

    public function setPassword(string $hashedPassword): void
    {
        $this->password = $hashedPassword;
    }

    public function isEmailConfirmed(): bool
    {
        return null !== $this->emailConfirmedAt;
    }

    public function getEmailConfirmedAt(): ?\DateTimeImmutable
    {
        return $this->emailConfirmedAt;
    }

    /** Only the first proof counts; a later one changes nothing. */
    public function confirmEmail(\DateTimeImmutable $at): void
    {
        $this->emailConfirmedAt ??= $at;
    }

    public function getCreatedAt(): \DateTimeImmutable
    {
        return $this->createdAt;
    }
}
