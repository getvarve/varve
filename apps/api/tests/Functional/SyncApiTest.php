<?php

declare(strict_types=1);

namespace App\Tests\Functional;

use App\Sync\Cursor;
use App\Sync\SyncScope;
use Symfony\Component\Uid\Uuid;

/**
 * Pushing and pulling a user's own records and their personal organization's (design 8.8).
 */
final class SyncApiTest extends DatabaseTestCase
{
    use AuthenticatesUsers;
    use SyncsRecords;

    private const string KIM = 'kim@example.test';
    private const string LISA = 'lisa@example.test';
    private const string USER_PUSH = '/api/v1/sync/user/push';
    private const string USER_CHANGES = '/api/v1/sync/user/changes';

    /** @var array<string, string> */
    private array $kim;
    private string $organization;

    protected function setUp(): void
    {
        parent::setUp();
        $this->bootstrapOAuthClients();
        $this->createUser(self::KIM, 'Kim Visser');
        $this->createUser(self::LISA, 'Lisa Bakker');
        $this->kim = $this->userHeaders(self::KIM);
        $this->organization = $this->organizationOf($this->kim);
    }

    public function testEveryKindMakesTheRoundTripAsItWasPushed(): void
    {
        $folder = self::newId();
        $connection = self::newId();
        $query = self::newId();
        $payloads = [
            $folder => self::folderRecord($folder),
            // A field this server does not know, an empty object and a float that is a whole number all come back as
            // they were sent.
            $connection => self::connectionProfile($connection, ['folderId' => $folder, 'fromANewerApp' => ['nested' => new \stdClass(), 'ratio' => 1.0]]),
            $query => self::savedQuery($query, ['folderId' => $folder, 'connectionId' => $connection]),
        ];

        $results = $this->push($this->path('push'), [
            self::upsert($folder, 'folder', $payloads[$folder]),
            self::upsert($connection, 'connection', $payloads[$connection]),
            self::upsert($query, 'query', $payloads[$query]),
        ], $this->kim);

        foreach ($results as $index => $result) {
            self::assertSame(['id' => array_keys($payloads)[$index], 'status' => 'accepted', 'version' => 1], $result);
        }

        $page = $this->pull($this->path('changes'), $this->kim);
        self::assertStringContainsString('"fromANewerApp":{"nested":{},"ratio":1.0}', (string) $this->client->getResponse()->getContent());
        self::assertFalse($page['hasMore']);
        self::assertSame([$folder, $connection, $query], array_column($page['changes'], 'id'));
        self::assertSame(['folder', 'connection', 'query'], array_column($page['changes'], 'kind'));
        $device = $this->currentDeviceId($this->kim);
        $kimId = $this->userId(self::KIM);
        foreach ($page['changes'] as $record) {
            $id = self::string($record['id'] ?? null);
            self::assertSame(1, $record['version'] ?? null);
            self::assertFalse($record['deleted'] ?? null);
            self::assertEquals(json_decode(json_encode($payloads[$id], \JSON_THROW_ON_ERROR | \JSON_PRESERVE_ZERO_FRACTION), true), $record['payload'] ?? null);
            self::assertSame($kimId, $record['updatedBy'] ?? null);
            self::assertSame($device, $record['updatedByDevice'] ?? null);
            self::assertMatchesRegularExpression('/^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d[+-]\d\d:\d\d$/', self::string($record['createdAt'] ?? null));
            self::assertSame($record['createdAt'], $record['updatedAt'] ?? null);
        }
        self::assertSame(['id', 'kind', 'version', 'deleted', 'payload', 'createdAt', 'updatedAt', 'updatedBy', 'updatedByDevice'], array_keys($page['changes'][0]));

        self::assertSame([], $this->pull($this->path('changes'), $this->kim, $page['cursor'])['changes'], 'Nothing new after the cursor.');
    }

    public function testUserRecordsMakeTheRoundTripToo(): void
    {
        $connection = self::newId();
        $favourite = self::newId();
        $preference = self::newId();
        $override = self::newId();

        $results = $this->push(self::USER_PUSH, [
            self::upsert($favourite, 'favourite', ['recordId' => $connection]),
            self::upsert($preference, 'preference', ['editor' => ['fontSize' => 13, 'theme' => 'dark']]),
            self::upsert($override, 'override', ['connectionId' => $connection, 'username' => 'kim_ro', 'ssh' => ['username' => 'kim']]),
        ], $this->kim);
        self::assertSame(['accepted', 'accepted', 'accepted'], array_column($results, 'status'));

        $page = $this->pull(self::USER_CHANGES, $this->kim);
        self::assertSame([$favourite, $preference, $override], array_column($page['changes'], 'id'));
        self::assertSame(['favourite', 'preference', 'override'], array_column($page['changes'], 'kind'));
        self::assertSame(['editor' => ['fontSize' => 13, 'theme' => 'dark']], $page['changes'][1]['payload'] ?? null);
        self::assertSame(['id', 'kind', 'version', 'deleted', 'payload', 'createdAt', 'updatedAt', 'updatedByDevice'], array_keys($page['changes'][0]), 'A user\'s own record does not say who wrote it.');

        self::assertSame([], $this->pull($this->path('changes'), $this->kim)['changes'], 'The organization\'s feed is another feed.');
    }

    public function testAnUpdateNeedsTheCurrentVersion(): void
    {
        $id = self::newId();
        $this->push($this->path('push'), [self::upsert($id, 'query', self::savedQuery($id))], $this->kim);
        self::assertSame(
            ['id' => $id, 'status' => 'accepted', 'version' => 2],
            $this->push($this->path('push'), [self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 2']), 1)], $this->kim)[0],
        );

        // Another device that last saw version 1.
        $stale = $this->push($this->path('push'), [self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 3']), 1)], $this->kim)[0];

        self::assertSame('conflict', $stale['status'] ?? null);
        $current = $stale['current'] ?? null;
        self::assertIsArray($current);
        self::assertSame(2, $current['version'] ?? null);
        self::assertSame('SELECT 2', self::payloadOf($current)['sql'] ?? null, 'The conflict carries the server\'s copy.');
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM record WHERE payload->>'sql' = 'SELECT 2'"), 'The stale change was not written.');

        $ahead = $this->push($this->path('push'), [self::upsert($id, 'query', self::savedQuery($id), 7)], $this->kim)[0];
        self::assertSame('conflict', $ahead['status'] ?? null, 'A version the server never had is a conflict too.');

        $unknown = self::newId();
        self::assertSame(
            ['id' => $unknown, 'status' => 'conflict', 'current' => null],
            $this->push($this->path('push'), [self::upsert($unknown, 'query', self::savedQuery($unknown), 3)], $this->kim)[0],
            'An update of a record the server does not have.',
        );
    }

    public function testCreatingWithATakenIdIsAConflictNotAnOverwrite(): void
    {
        $id = self::newId();
        $this->push($this->path('push'), [self::upsert($id, 'connection', self::connectionProfile($id, ['name' => 'First']))], $this->kim);

        $second = $this->push($this->path('push'), [self::upsert($id, 'connection', self::connectionProfile($id, ['name' => 'Second']))], $this->kim)[0];

        self::assertSame('conflict', $second['status'] ?? null);
        self::assertIsArray($second['current'] ?? null);
        self::assertSame('First', self::payloadOf($second['current'])['name'] ?? null);

        $this->push($this->path('push'), [self::delete($id, 1)], $this->kim);
        $afterDelete = $this->push($this->path('push'), [self::upsert($id, 'connection', self::connectionProfile($id, ['name' => 'Third']))], $this->kim)[0];
        self::assertSame('conflict', $afterDelete['status'] ?? null, 'A tombstone holds its id as well.');
        self::assertIsArray($afterDelete['current'] ?? null);
        self::assertTrue($afterDelete['current']['deleted'] ?? null);

        self::assertSame(
            ['id' => $id, 'status' => 'accepted', 'version' => 3],
            $this->push($this->path('push'), [self::upsert($id, 'connection', self::connectionProfile($id, ['name' => 'Back']), 2)], $this->kim)[0],
            'A client that saw the delete may bring the record back.',
        );
        self::assertSame(1, $this->countRows("SELECT COUNT(*) FROM record WHERE deleted_at IS NULL AND payload->>'name' = 'Back'"));
    }

    public function testAKindCannotChange(): void
    {
        $id = self::newId();
        $this->push($this->path('push'), [self::upsert($id, 'folder', self::folderRecord($id))], $this->kim);

        $result = $this->push($this->path('push'), [self::upsert($id, 'query', self::savedQuery($id), 1)], $this->kim)[0];

        self::assertSame(['id' => $id, 'status' => 'rejected', 'reason' => 'invalid', 'field' => 'kind', 'message' => 'This record is a folder; its kind cannot change.'], $result);
    }

    public function testADeleteLeavesATombstoneThatThePullReports(): void
    {
        $kept = self::newId();
        $deleted = self::newId();
        $this->push($this->path('push'), [self::upsert($kept, 'folder', self::folderRecord($kept)), self::upsert($deleted, 'folder', self::folderRecord($deleted))], $this->kim);
        $cursor = $this->pull($this->path('changes'), $this->kim)['cursor'];

        self::assertSame(['id' => $deleted, 'status' => 'accepted', 'version' => 2], $this->push($this->path('push'), [self::delete($deleted, 1)], $this->kim)[0]);

        $page = $this->pull($this->path('changes'), $this->kim, $cursor);
        self::assertCount(1, $page['changes']);
        $tombstone = $page['changes'][0];
        self::assertSame($deleted, $tombstone['id'] ?? null);
        self::assertSame('folder', $tombstone['kind'] ?? null);
        self::assertSame(2, $tombstone['version'] ?? null);
        self::assertTrue($tombstone['deleted'] ?? null);
        self::assertArrayHasKey('payload', $tombstone);
        self::assertNull($tombstone['payload']);

        self::assertSame([$kept], array_column($this->pull($this->path('changes'), $this->kim)['changes'], 'id'), 'A first pull gets the live records only.');

        self::assertSame(['id' => $deleted, 'status' => 'accepted', 'version' => 2], $this->push($this->path('push'), [self::delete($deleted, 2)], $this->kim)[0], 'Deleting again changes nothing.');
        self::assertSame([], $this->pull($this->path('changes'), $this->kim, $page['cursor'])['changes']);
        self::assertSame('conflict', $this->push($this->path('push'), [self::delete($kept, 4)], $this->kim)[0]['status'] ?? null);

        $unknown = self::newId();
        self::assertSame(
            ['id' => $unknown, 'status' => 'rejected', 'reason' => 'not_found', 'field' => null, 'message' => 'There is no record with this id to delete.'],
            $this->push($this->path('push'), [self::delete($unknown, 0)], $this->kim)[0],
        );
    }

    public function testChangesOfOnePushApplyInOrder(): void
    {
        $id = self::newId();

        $results = $this->push($this->path('push'), [
            self::upsert($id, 'folder', self::folderRecord($id)),
            self::upsert($id, 'folder', self::folderRecord($id, ['name' => 'Renamed']), 1),
            self::upsert($id, 'folder', self::folderRecord($id, ['name' => 'Stale']), 1),
            self::delete($id, 2),
        ], $this->kim);

        self::assertSame(['accepted', 'accepted', 'conflict', 'accepted'], array_column($results, 'status'));
        self::assertSame([1, 2, null, 3], array_map(static fn (array $result): mixed => $result['version'] ?? null, $results));
    }

    public function testPagesCoverEveryRecordOnce(): void
    {
        $ids = [];
        $changes = [];
        for ($i = 0; $i < 25; ++$i) {
            $ids[] = $id = self::newId();
            $changes[] = self::upsert($id, 'folder', self::folderRecord($id, ['name' => 'Folder '.$i]));
        }
        $this->push($this->path('push'), $changes, $this->kim);

        $first = $this->pull($this->path('changes'), $this->kim, limit: 10);
        self::assertCount(10, $first['changes']);
        self::assertTrue($first['hasMore']);
        $all = $this->pullAll($this->path('changes'), $this->kim, limit: 10);

        self::assertSame(3, $all['pages']);
        self::assertSame($ids, array_column($all['changes'], 'id'));
        self::assertSame([], $this->pull($this->path('changes'), $this->kim, $all['cursor'])['changes']);
    }

    /**
     * Writes between the pages of a first pull: a record already pulled comes again with its new version, one deleted
     * after it was pulled comes as a tombstone, and a new one comes too. Nothing is lost.
     */
    public function testTheCursorHoldsWhileRecordsChangeBetweenPages(): void
    {
        $ids = [];
        $changes = [];
        for ($i = 0; $i < 12; ++$i) {
            $ids[] = $id = self::newId();
            $changes[] = self::upsert($id, 'folder', self::folderRecord($id, ['name' => 'Folder '.$i]));
        }
        $this->push($this->path('push'), $changes, $this->kim);
        $deletedBefore = self::newId();
        $this->push($this->path('push'), [self::upsert($deletedBefore, 'folder', self::folderRecord($deletedBefore))], $this->kim);
        $this->push($this->path('push'), [self::delete($deletedBefore, 1)], $this->kim);

        $first = $this->pull($this->path('changes'), $this->kim, limit: 5);
        self::assertSame(\array_slice($ids, 0, 5), array_column($first['changes'], 'id'));

        $new = self::newId();
        $this->push($this->path('push'), [
            self::upsert($ids[1], 'folder', self::folderRecord($ids[1], ['name' => 'Renamed while paging']), 1),
            self::delete($ids[2], 1),
            self::upsert($ids[8], 'folder', self::folderRecord($ids[8], ['name' => 'Ahead of the cursor']), 1),
            self::upsert($new, 'folder', self::folderRecord($new)),
        ], $this->kim);

        $rest = $this->pullAll($this->path('changes'), $this->kim, $first['cursor'], 5);

        $seen = [];
        foreach ([...$first['changes'], ...$rest['changes']] as $record) {
            $seen[self::string($record['id'] ?? null)] = $record;
        }
        self::assertArrayNotHasKey($deletedBefore, $seen, 'A tombstone from before the first pull is not news to it.');
        self::assertEqualsCanonicalizing([...$ids, $new], array_keys($seen));
        self::assertSame('Renamed while paging', self::payloadOf($seen[$ids[1]])['name'] ?? null);
        self::assertTrue($seen[$ids[2]]['deleted'] ?? null, 'Deleted after the first page pulled it.');
        self::assertSame('Ahead of the cursor', self::payloadOf($seen[$ids[8]])['name'] ?? null);
        self::assertSame(2, $seen[$ids[8]]['version'] ?? null);
        self::assertSame([], $this->pull($this->path('changes'), $this->kim, $rest['cursor'])['changes']);
    }

    public function testUsersNeverSeeEachOthersRecords(): void
    {
        $id = self::newId();
        $this->push($this->path('push'), [self::upsert($id, 'connection', self::connectionProfile($id))], $this->kim);
        $favourite = self::newId();
        $this->push(self::USER_PUSH, [self::upsert($favourite, 'favourite', ['recordId' => $id])], $this->kim);
        $lisa = $this->userHeaders(self::LISA);
        $lisasOrganization = $this->organizationOf($lisa);

        foreach ([$this->organization, Uuid::v7()->toRfc4122(), 'not-an-id'] as $organization) {
            $this->client->request('GET', self::organizationPath($organization, 'changes'), server: $lisa);
            self::assertResponseStatusCodeSame(404, $organization);
            $this->client->jsonRequest('POST', self::organizationPath($organization, 'push'), ['changes' => [self::upsert($id, 'connection', self::connectionProfile($id, ['name' => 'Lisa was here']), 1)]], $lisa);
            self::assertResponseStatusCodeSame(404, $organization);
            $this->client->request('GET', self::organizationPath($organization, 'records/'.$id.'/revisions'), server: $lisa);
            self::assertResponseStatusCodeSame(404, $organization);
        }

        // Kim's ids are no secret to Lisa: in her own organization they are just ids.
        self::assertSame(['id' => $id, 'status' => 'accepted', 'version' => 1], $this->push(self::organizationPath($lisasOrganization, 'push'), [self::upsert($id, 'connection', self::connectionProfile($id, ['name' => 'Lisa\'s own']))], $lisa)[0]);
        self::assertSame('accepted', $this->push(self::USER_PUSH, [self::upsert($favourite, 'favourite', ['recordId' => $id])], $lisa)[0]['status'] ?? null);
        self::assertSame(['Lisa\'s own'], array_map(static fn (array $record): mixed => self::payloadOf($record)['name'] ?? null, $this->pull(self::organizationPath($lisasOrganization, 'changes'), $lisa)['changes']));
        self::assertCount(1, $this->pull(self::USER_CHANGES, $lisa)['changes']);

        $kims = $this->pull($this->path('changes'), $this->kim)['changes'];
        self::assertCount(1, $kims);
        self::assertSame('Shop production', self::payloadOf($kims[0])['name'] ?? null);
        self::assertSame(1, $kims[0]['version'] ?? null);
        $this->client->request('GET', self::organizationPath($lisasOrganization, 'changes'), server: $this->kim);
        self::assertResponseStatusCodeSame(404);
    }

    public function testATokenWithoutTheSyncScopeIsRefused(): void
    {
        $accountOnly = $this->headersWithScope(self::KIM, 'account');
        $this->client->request('GET', '/api/v1/me', server: $accountOnly);
        self::assertResponseIsSuccessful('The token itself works.');

        foreach ([['GET', $this->path('changes')], ['POST', $this->path('push')], ['GET', self::USER_CHANGES], ['POST', self::USER_PUSH]] as [$method, $path]) {
            $this->client->jsonRequest($method, $path, 'POST' === $method ? ['changes' => []] : [], $accountOnly);
            self::assertResponseStatusCodeSame(403, $path);
        }

        $this->client->request('GET', $this->path('changes'));
        self::assertResponseStatusCodeSame(401);
    }

    public function testCursorsAreCheckedAndBoundToTheirFeed(): void
    {
        $id = self::newId();
        $this->push($this->path('push'), [self::upsert($id, 'folder', self::folderRecord($id))], $this->kim);
        $organizationCursor = $this->pull($this->path('changes'), $this->kim)['cursor'];
        $userCursor = $this->pull(self::USER_CHANGES, $this->kim)['cursor'];

        foreach (['garbage!', base64_encode('1:o00000000-0000-0000-0000-000000000000:1:0'), $userCursor] as $cursor) {
            $this->client->request('GET', $this->path('changes').'?cursor='.urlencode($cursor), server: $this->kim);
            self::assertResponseStatusCodeSame(422, $cursor);
        }
        $this->client->request('GET', self::USER_CHANGES.'?cursor='.$organizationCursor, server: $this->kim);
        self::assertResponseStatusCodeSame(422);

        foreach (['0', '1001', 'ten'] as $limit) {
            $this->client->request('GET', $this->path('changes').'?limit='.$limit, server: $this->kim);
            self::assertResponseStatusCodeSame(422, $limit);
        }

        // A cursor past what the server has (a server restored from an older backup) asks for a full pull.
        $ahead = new Cursor(99)->encode(SyncScope::organization(Uuid::fromString($this->organization)));
        $this->client->request('GET', $this->path('changes').'?cursor='.$ahead, server: $this->kim);
        self::assertResponseStatusCodeSame(410);
    }

    public function testRevisionsOfASavedQuery(): void
    {
        $id = self::newId();
        $changes = [self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 1']))];
        for ($version = 1; $version <= 52; ++$version) {
            $changes[] = self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT '.($version + 1)]), $version);
        }
        $this->push($this->path('push'), $changes, $this->kim);

        $this->client->request('GET', $this->path('records/'.$id.'/revisions'), server: $this->kim);

        self::assertResponseIsSuccessful();
        $revisions = $this->decodedResponse()['revisions'] ?? null;
        self::assertIsArray($revisions);
        self::assertCount(50, $revisions, 'The last 50 are kept.');
        self::assertIsArray($revisions[0]);
        self::assertSame(52, $revisions[0]['version'] ?? null, 'Newest first; the current version 53 is the record itself.');
        self::assertSame('SELECT 52', self::payloadOf($revisions[0])['sql'] ?? null);
        self::assertSame(['version', 'payload', 'updatedAt', 'updatedBy', 'updatedByDevice'], array_keys($revisions[0]));
        self::assertIsArray($revisions[49]);
        self::assertSame(3, $revisions[49]['version'] ?? null);

        $this->push($this->path('push'), [self::delete($id, 53)], $this->kim);
        $this->client->request('GET', $this->path('records/'.$id.'/revisions'), server: $this->kim);
        $afterDelete = $this->decodedResponse()['revisions'] ?? null;
        self::assertIsArray($afterDelete);
        self::assertIsArray($afterDelete[0]);
        self::assertSame(53, $afterDelete[0]['version'] ?? null, 'A delete keeps what it removed.');

        $folder = self::newId();
        $this->push($this->path('push'), [self::upsert($folder, 'folder', self::folderRecord($folder)), self::upsert($folder, 'folder', self::folderRecord($folder, ['name' => 'Renamed']), 1)], $this->kim);
        $this->client->request('GET', $this->path('records/'.$folder.'/revisions'), server: $this->kim);
        self::assertSame(['revisions' => []], $this->decodedResponse(), 'Only saved queries keep revisions.');

        $this->client->request('GET', $this->path('records/'.self::newId().'/revisions'), server: $this->kim);
        self::assertResponseStatusCodeSame(404);
    }

    public function testPushesAndPullsAreRateLimitedPerOrganizationAndPerUser(): void
    {
        // In tests: 20 per organization, 30 per user (config/packages/rate_limiter.yaml).
        for ($i = 0; $i < 20; ++$i) {
            $this->push($this->path('push'), [], $this->kim);
        }
        $this->client->jsonRequest('POST', $this->path('push'), ['changes' => []], $this->kim);
        self::assertResponseStatusCodeSame(429);
        self::assertTrue($this->client->getResponse()->headers->has('Retry-After'));
        for ($i = 0; $i < 9; ++$i) {
            $this->push(self::USER_PUSH, [], $this->kim);
        }
        $this->client->jsonRequest('POST', self::USER_PUSH, ['changes' => []], $this->kim);
        self::assertResponseStatusCodeSame(429, 'The 21 organization pushes counted for the user too.');

        for ($i = 0; $i < 20; ++$i) {
            $this->pull($this->path('changes'), $this->kim);
        }
        $this->client->request('GET', $this->path('changes'), server: $this->kim);
        self::assertResponseStatusCodeSame(429);
        for ($i = 0; $i < 9; ++$i) {
            $this->pull(self::USER_CHANGES, $this->kim);
        }
        $this->client->request('GET', self::USER_CHANGES, server: $this->kim);
        self::assertResponseStatusCodeSame(429);

        $lisa = $this->userHeaders(self::LISA);
        $this->pull(self::USER_CHANGES, $lisa);
        $this->push(self::USER_PUSH, [], $lisa);
    }

    public function testDeletingTheAccountDeletesItsRecords(): void
    {
        $id = self::newId();
        $this->push($this->path('push'), [self::upsert($id, 'query', self::savedQuery($id)), self::upsert($id, 'query', self::savedQuery($id, ['sql' => 'SELECT 2']), 1)], $this->kim);
        $this->push(self::USER_PUSH, [self::upsert(self::newId(), 'preference', ['theme' => 'dark'])], $this->kim);
        $lisa = $this->userHeaders(self::LISA);
        $this->push(self::organizationPath($this->organizationOf($lisa), 'push'), [self::upsert($id, 'query', self::savedQuery($id))], $lisa);
        $this->push(self::USER_PUSH, [self::upsert(self::newId(), 'preference', ['theme' => 'light'])], $lisa);

        $this->client->jsonRequest('POST', '/api/v1/me/deletion', ['password' => 'correct-horse-battery-staple'], $this->kim);

        self::assertResponseStatusCodeSame(204);
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM record'), 'Only Lisa\'s record is left.');
        self::assertSame(0, $this->countRows('SELECT COUNT(*) FROM record_revision'));
        self::assertSame(1, $this->countRows('SELECT COUNT(*) FROM user_record'));
    }

    public function testSigningOutADeviceKeepsWhatItWrote(): void
    {
        $tokens = $this->signInDevice(self::KIM, 'Old laptop');
        $old = self::bearer($tokens['access_token']);
        $id = self::newId();
        $this->push($this->path('push'), [self::upsert($id, 'folder', self::folderRecord($id))], $old);
        $device = $this->currentDeviceId($old);

        $this->client->request('DELETE', '/api/v1/devices/'.$device, server: $this->kim);
        self::assertResponseStatusCodeSame(204);

        $records = $this->pull($this->path('changes'), $this->kim)['changes'];
        self::assertCount(1, $records);
        self::assertArrayHasKey('updatedByDevice', $records[0]);
        self::assertNull($records[0]['updatedByDevice']);
    }

    private function path(string $action): string
    {
        return self::organizationPath($this->organization, $action);
    }

    /**
     * @param array<string, string> $headers
     */
    private function currentDeviceId(array $headers): string
    {
        $this->client->request('GET', '/api/v1/devices', server: $headers);
        foreach ($this->decodedResponse() as $device) {
            self::assertIsArray($device);
            if (true === ($device['current'] ?? null)) {
                return self::string($device['id'] ?? null);
            }
        }
        self::fail('No current device.');
    }

    private function userId(string $email): string
    {
        return self::string($this->connection()->fetchOne('SELECT id FROM app_user WHERE email = :email', ['email' => $email]));
    }

    /**
     * Headers of a desktop sign-in that asked for these scopes only.
     *
     * @return array<string, string>
     */
    private function headersWithScope(string $email, string $scope): array
    {
        $verifier = bin2hex(random_bytes(32));
        $this->client->getCookieJar()->clear();
        $crawler = $this->client->request('GET', '/login');
        $this->client->submit($crawler->selectButton('Sign in')->form(['_username' => $email, '_password' => 'correct-horse-battery-staple']));
        $this->client->request('GET', $this->desktopAuthorizeUrl($verifier, ['scope' => $scope]));
        $code = $this->codeFrom((string) $this->client->getResponse()->headers->get('Location'));
        $this->client->request('POST', '/oauth/token', $this->codeExchange($code, $verifier));
        self::assertResponseIsSuccessful();
        $this->client->getCookieJar()->clear();

        return self::bearer(self::string($this->decodedResponse()['access_token'] ?? null));
    }

    /**
     * @param array<mixed> $record
     *
     * @return array<mixed>
     */
    private static function payloadOf(array $record): array
    {
        $payload = $record['payload'] ?? null;
        self::assertIsArray($payload);

        return $payload;
    }
}
