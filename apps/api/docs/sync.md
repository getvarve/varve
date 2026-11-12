# Sync wire format

How a client keeps its records in sync with a Varve server (design 8.6 to 8.8). The desktop app's cloud client is
written against this document; when the server changes what is here, this document changes with it.

## Feeds

There are two kinds of feed, each with its own records, version numbers and cursor:

| Feed | Holds | Kinds | Push | Pull |
|---|---|---|---|---|
| Organization | the records of one organization: today only the signed-in user's personal organization | `folder`, `connection`, `query` | `POST /api/v1/sync/organizations/{organizationId}/push` | `GET /api/v1/sync/organizations/{organizationId}/changes` |
| User | the signed-in user's own records, which only they see | `favourite`, `preference`, `override` | `POST /api/v1/sync/user/push` | `GET /api/v1/sync/user/changes` |

`{organizationId}` is `personalOrganization.id` from `GET /api/v1/me`. Any other organization id, existing or not, is a
404.

The client keeps one cursor per organization feed plus one for the user feed. A sync round is: push the outbox of a
feed, then pull that feed from its cursor.

Every request needs an access token with the `sync` scope (`Authorization: Bearer <token>`, `Accept:
application/json`). The revisions of a saved query are at
`GET /api/v1/sync/organizations/{organizationId}/records/{recordId}/revisions`.

## Records

A record as the server returns it, in a pull and as the server's copy in a conflict:

```json
{
  "id": "019a7c1e-6f00-7b2a-9d4e-0c1f2a3b4c5d",
  "kind": "connection",
  "version": 3,
  "deleted": false,
  "payload": { "name": "Shop production", "engine": "postgres", "...": "..." },
  "createdAt": "2026-11-12T13:00:00+00:00",
  "createdBy": "019a7b00-0000-7000-8000-000000000001",
  "updatedAt": "2026-11-12T14:30:00+00:00",
  "updatedBy": "019a7b00-0000-7000-8000-000000000001",
  "updatedByDevice": "019a7b00-0000-7000-8000-0000000000aa"
}
```

| Field | Meaning |
|---|---|
| `id` | Chosen by the client: a UUID in lower case with hyphens (the desktop app uses v7). Unique within its feed only. |
| `kind` | One of the feed's kinds. Never changes. |
| `version` | Counts the accepted writes of this record: 1 after the create, one more for every accepted upsert or delete. The `baseVersion` of the client's next change. |
| `deleted` | `true` for a tombstone: the record was deleted. |
| `payload` | The JSON object the client pushed last; `null` for a tombstone. |
| `createdAt`, `updatedAt` | Server time of the create and of the last write, RFC 3339 with offset, to the second. Bringing a deleted record back keeps its `createdAt`. |
| `createdBy` | Organization feed only: id of the user who created the record; `null` once that user is deleted. Bringing a deleted record back keeps its creator. The user feed has no such field. |
| `updatedBy` | Organization feed only: id of the user who wrote the last change; `null` once that user is deleted. The user feed has no such field. |
| `updatedByDevice` | Id of the device (`GET /api/v1/devices`) that wrote the last change; `null` once it is signed out. |

Payloads come back as they were pushed, except for whitespace and the order of keys: an empty object stays `{}`, a
float stays a float (`1.0`), and fields the server does not know are kept.

## Push

```
POST /api/v1/sync/organizations/{organizationId}/push
POST /api/v1/sync/user/push
Content-Type: application/json
```

```json
{
  "changes": [
    { "op": "upsert", "id": "<uuid>", "kind": "query", "baseVersion": 0, "payload": { "name": "Open orders", "sql": "SELECT 1" } },
    { "op": "delete", "id": "<uuid>", "baseVersion": 4 }
  ]
}
```

| Field | Rules |
|---|---|
| `op` | `"upsert"` or `"delete"`. |
| `id` | The record's id: lower-case UUID with hyphens. |
| `baseVersion` | The `version` of the record the change is based on, as the client last saw it; `0` to create a record. An integer from 0 to 2147483647. |
| `kind` | Upsert only, required: one of the feed's kinds. |
| `payload` | Upsert only, required: a JSON object (see [Payloads](#payloads)). |

A delete carries `op`, `id` and `baseVersion` only. Any other field in a change rejects that change.

### Answer

`200 OK` with one result per change, in the order of the changes:

```json
{
  "results": [
    { "id": "<uuid>", "status": "accepted", "version": 1 },
    { "id": "<uuid>", "status": "conflict", "current": { "id": "<uuid>", "kind": "query", "version": 5, "deleted": false, "payload": {}, "createdAt": "...", "createdBy": "...", "updatedAt": "...", "updatedBy": "...", "updatedByDevice": "..." } },
    { "id": "<uuid>", "status": "rejected", "reason": "secret", "field": "payload.ssh.password", "message": "payload.ssh.password looks like a secret. Passwords, passphrases, keys and tokens never leave the device." },
    { "id": "<uuid>", "status": "rejected", "reason": "too_large", "field": "payload", "message": "payload is 400123 bytes as JSON; at most 262144 are allowed.", "limit": 262144, "size": 400123 }
  ]
}
```

| Status | Fields | Meaning |
|---|---|---|
| `accepted` | `id`, `version` | Stored; `version` is the record's new version. The record also has a new place in the feed, so the next pull returns it (the client's own write comes back to it). |
| `conflict` | `id`, `current` | `baseVersion` is not the record's version. `current` is the server's copy in the [record shape](#records), a tombstone included, or `null` when the server has no record with this id (it never had one, or its tombstone was purged). Nothing was written. |
| `rejected` | `id`, `reason`, `field`, `message`, sometimes `limit` and `size` | The server will not store this change, whatever the client retries. `id` is `null` when the change had no valid id. `field` is the path of the cause inside the change (`id`, `op`, `baseVersion`, `kind`, `payload`, `payload.tls.mode`, `payload.tags[2]`), or `null`. A path longer than 200 bytes is cut to its first 200 bytes followed by `...`, in `field` and inside `message`. `message` is for people. `limit` and `size` are numbers, present for the reasons below that name them and absent otherwise. |

Reasons of a rejection:

| `reason` | When | Extra fields |
|---|---|---|
| `invalid` | The change or its payload breaks a rule of the envelope or of its kind; also an upsert that would change the kind of an existing record (`field` is `kind`). | |
| `unknown_kind` | `kind` is not one of the feed's kinds. | |
| `secret` | The payload holds a secret (see [Secrets](#secrets)). | |
| `too_large` | The payload is larger than 262144 bytes as the server stores it (see [Payloads](#payloads)). | `limit`: 262144; `size`: the bytes of this payload |
| `not_found` | A delete of a record the server does not have. | |
| `record_limit` | The upsert would create or bring back a record while the feed is at its limit of live records. | `limit`: the most live records the feed may hold |
| `storage_limit` | The upsert would create, bring back or enlarge a record, and the payloads of the feed's live records would then add up to more bytes than the feed may hold. An upsert that makes a record smaller or keeps its size is never rejected for this. | `limit`: the most bytes the feed may hold |

### What a client does with each result

- `accepted`: store `version` as the record's version and drop the change from the outbox.
- `conflict`: nothing was written. Take `current` as the server's copy and resolve (last write wins or keep both, design
  8.8), then push again on `current.version`; with `current: null` the server has no such record, so a create
  (`baseVersion` 0) is the way to keep it.
- `rejected`: the change is not stored and pushing the same change again gives the same answer. Keep the record on the
  device, mark it as not synced with the reason, and do not push it again until the user changes it (for
  `record_limit` and `storage_limit`: until the user changes it or makes room by deleting or shrinking other records).
- `too_large` in particular: the desktop app accepts saved queries of up to 1 MiB of SQL, the server stores at most
  256 KiB per payload. Such a record stays on the device only: keep it local, mark it as not synced and show `size`
  and `limit` to the user, and do not push it again until it has changed. Once it fits, it syncs like any other
  record.
- A `status` or `reason` this document does not list: treat it as "not accepted, keep local", exactly like
  `rejected`. Keep the record on the device, mark it as not synced, and do not push the same change again until it
  changes. Later versions of the server add statuses and reasons, and a client must not fail on them or retry them in
  a loop.

Reserved for later versions, so clients written today already handle them by the rule above:

| Value | Where | Meaning when it arrives |
|---|---|---|
| `forbidden` | `status` (with `id`; its other fields come with the version that adds it) | Team organizations (design 8.8): the user's role does not allow this write. The client keeps the change as a local draft. |
| `forbidden` | `reason` of a `rejected` result | Reserved together with the status; not used by this version. |

### Rules

- The changes apply in their order, as if pushed one by one: a later change of the same record is based on the version
  an earlier one produced.
- A push is one transaction that holds the feed's lock: pushes to one feed take turns, each sees every change committed
  before it, and two writers can never both write on top of the same version. Changes that are not accepted do not
  stop the others; only an error of the whole request (below) stores nothing.
- Create: `baseVersion` 0 and no record with this id. When the id is taken, by a live record or by a tombstone, the
  answer is a conflict with that record: a create never overwrites.
- Update: `baseVersion` equal to the record's `version`. Upserting a tombstone with its version brings the record back.
- Delete: `baseVersion` equal to the record's `version`; leaves a tombstone without payload at the next version.
  Deleting a tombstone again with its version is accepted and changes nothing. Deleting an id the server does not have
  is rejected with `not_found`.
- A retried push whose first answer was lost gets conflicts whose `current` is what it pushed, from this device
  (`updatedByDevice`): the client can treat those as accepted.
- Every accepted upsert or delete of a `query` keeps the payload it replaced as a revision (see
  [Revisions](#revisions)). Bringing a deleted record back keeps no revision, since a tombstone has no payload.
- A record's size counts for `storage_limit` while it is live; a tombstone counts for nothing, and revisions are
  capped per record instead.

### Errors of the whole request

These store nothing. Bodies are `application/problem+json`.

| Status | When | Body |
|---|---|---|
| 400 | The body is not JSON. | `{"title": "An error occurred", "detail": "The body is not valid JSON.", "status": 400, "type": "/errors/400"}` |
| 401 | No token, or the token or its device is no longer valid. | not JSON |
| 403 | The token has no `sync` scope. | problem JSON |
| 404 | Not the signed-in user's personal organization; or the account or organization was deleted while the request was on its way (`"detail": "User not found."` on the user feed). | `{"title": "An error occurred", "detail": "Organization not found.", "status": 404, "type": "/errors/404"}` |
| 413 | The body is larger than 8388608 bytes (8 MiB), by its `Content-Length` or, without one, by what arrived. Split the changes over more pushes. | `{"title": "An error occurred", "detail": "A push can be at most 8388608 bytes; send the changes in more than one push.", "status": 413, "type": "/errors/413"}` |
| 415 | The body is not sent as `application/json`. | problem JSON |
| 422 | The body is not an object with a list `changes`, or the list has more than 100 changes, or the name of a field anywhere in the body starts with a NUL character (`"\u0000a"`), which the server cannot read. | `{"status": 422, "violations": [{"propertyPath": "changes", "message": "A push can hold at most 100 changes.", "code": null}], "detail": "changes: A push can hold at most 100 changes.", "type": "/validation_errors/...", "title": "An error occurred"}` |
| 429 | Too many pushes (see [Limits](#limits)). Wait the seconds in `Retry-After`. | `{"title": "An error occurred", "detail": "Too many sync requests. Try again later.", "status": 429, "type": "/errors/429"}` |

The order of the checks: token and scope (401, 403), the limit per user (429), the organization (404), the limit per
organization (429), then the body (415, 413, 400, 422). Requests for an organization that is not the user's count
against the user's limit.

A development server adds a `trace` to problem bodies; clients ignore unknown fields.

## Pull

```
GET /api/v1/sync/organizations/{organizationId}/changes?cursor=<cursor>&limit=<n>
GET /api/v1/sync/user/changes?cursor=<cursor>&limit=<n>
```

| Parameter | Rules |
|---|---|
| `cursor` | The `cursor` of the previous answer for this feed. Leave it out for a first pull. |
| `limit` | Records per page at most, 1 to 1000; 200 when left out. A page also holds no more records than fit in 4194304 bytes (4 MiB) of payload, but always at least one. |

Answer, `200 OK`:

```json
{
  "changes": [ { "id": "<uuid>", "kind": "folder", "version": 2, "deleted": false, "payload": { "name": "Webshop" }, "createdAt": "...", "createdBy": "...", "updatedAt": "...", "updatedBy": "...", "updatedByDevice": "..." } ],
  "cursor": "MTpvMDE5YTdjMWUtNmYwMC03YjJhLTlkNGUtMGMxZjJhM2I0YzVkOjQ3OjA",
  "hasMore": false
}
```

- `changes` holds every record written after the cursor, once, as it is now, in the order of their last write;
  tombstones included (`deleted: true`). A record written again later comes again.
- Without a cursor the pull returns every live record and no tombstones; it pages like any other pull, and a record
  deleted while the client pages through it does come as a tombstone.
- Store `cursor` once the page is applied, and pull again with it while `hasMore` is `true`. With `hasMore: false` the
  client is up to date; later pulls with that cursor return what changed since.
- The cursor is opaque: store it as a string and send it back unchanged. It belongs to one feed, and stays valid until
  the server purges a tombstone the client has not seen.
- The client applies a tombstone by deleting its local copy (a record it does not have needs nothing).
- A pull reads one state of the server. A cursor from before a tombstone purge never gets an answer that leaves the
  purged delete out: it gets the 410 below.

Errors:

| Status | When | Body |
|---|---|---|
| 401, 403, 404, 429 | As for a push, in the same order. | |
| 410 | The cursor is too old: tombstones after it were purged (90 days after their delete), or it is from a state the server does not have: its number is past the feed's, as after a restore from an older backup. Pull everything again without a cursor and replace the local copy of the feed with it, keeping local changes that are not pushed yet. | `{"title": "An error occurred", "detail": "This cursor is too old: deletes after it were purged. Pull everything again without a cursor.", "status": 410, "type": "/errors/410"}` |
| 422 | `cursor` is not a cursor of this feed, or `limit` is not a whole number from 1 to 1000. Also when either is given as a list (`?cursor[]=...`). | `{"status": 422, "violations": [{"propertyPath": "cursor", "message": "This is not a cursor of these records.", "code": null}], ...}` |

A restore from backup is only noticed while the cursor's number is past the restored feed's. Once a restored server
has accepted as many writes as it lost, an old cursor is in range again and the client is not told: it keeps records
the server lost (pushing a change to one is answered with a conflict and `current: null`), and it never pulls the
writes the server accepted between the restore and the moment the feed's number passed the cursor's. The server has
no way to tell such a cursor from a good one. After restoring a backup, have every client pull again without a
cursor.

## Revisions

```
GET /api/v1/sync/organizations/{organizationId}/records/{recordId}/revisions
```

`200 OK` with the payloads a saved query had before, newest first:

```json
{
  "revisions": [
    { "version": 4, "payload": { "name": "Open orders", "sql": "SELECT 1" }, "updatedAt": "2026-11-12T14:30:00+00:00", "updatedBy": "<user id or null>", "updatedByDevice": "<device id or null>" }
  ]
}
```

`version` is the version that payload had; the record itself holds the current one. A record of another kind has an
empty list; a record the organization does not have (or no longer has, after its tombstone was purged) is a 404, as is
a `recordId` that is not a lower-case UUID. To restore a revision, push it as an upsert on the current version. Counts
as a pull for the limits; 401, 403, 404 and 429 are as for a pull.

The server keeps the newest revisions of a record: at most `VARVE_SYNC_MAX_REVISIONS` of them (50 unless the server
sets it) and at most `VARVE_SYNC_MAX_REVISION_BYTES` bytes of payload together (8388608, 8 MiB, unless the server sets
it); older ones are dropped when a new one is kept. The newest revision always stays. The answer is not paged, so it
can be a little over 8 MiB with those settings. Revisions go when their record's tombstone is purged.

## Payloads

Every payload is a JSON object of at most 262144 bytes (256 KiB) as the server stores it: compact JSON (no white space
between tokens) with slashes and non-ASCII characters unescaped. A larger one is rejected with `too_large`, which
carries that size as `size`. Objects and lists nest at most 32 levels below the payload: the payload is level 0, an
object or list directly in it level 1, and one at level 33 is rejected. Numbers must lie within the signed 64-bit
integer range, -2^63 to 2^63 - 1; any other number (`1e19`, `1e999`) is rejected, since it would not come back as it
was sent.

The server does not read SQL or connect anywhere. It checks the fields below and keeps every other field as it came.
References to other records are checked for their shape only, since a record may arrive before the folder it is in.
Where `id` is in the payload, it must be the change's `id`.

| Kind | Required | Checked when present |
|---|---|---|
| `folder` | `name`: a string, not blank, at most 200 characters | `parentId`: null or a record id, and not the folder's own id; `color`: null or a string of at most 32 characters; `sortOrder`: null or an integer |
| `connection` | `name` as above; `engine`: lower-case letters, digits and `_`, at most 32 (`postgres`, `mysql`, `mariadb`, `redis`) | `folderId`: null or a record id; `color`, `sortOrder` as above; `host`: a string of at most 255 bytes; `port`: null or an integer from 0 to 65535; `database`, `username`: null or a string of at most 255 bytes; `tls`, `ssh`, `options`: null or an object |
| `query` | `name` as above; `sql`: a string (may be empty) | `folderId`, `connectionId`: null or a record id; `color`, `sortOrder` as above; `database`: null or a string of at most 255 bytes; `tags`: null or a list of at most 100 strings, each not blank and at most 100 characters |
| `favourite` | `recordId`: a record id | |
| `override` | `connectionId`: a record id | |
| `preference` | | |

A record id is a lower-case UUID with hyphens.

### Secrets

Secrets never reach the server (design 8.4 and 8.8): passwords, passphrases, keys and tokens stay in each device's
keychain. A payload that holds one is rejected with reason `secret` and the path of the field. The guard catches
client bugs; it is not meant to stop a client that tries to get around it.

**By the name of a field**, at any depth, unless its value is `null`, `true` or `false`. The name is compared after
dropping everything but ASCII letters and digits, lower-casing, and then dropping digits at the end and one closing
`value` (so `password_2` and `tokenValue` count as `password` and `token`). What is left is a secret when it

- ends in `password`, `passwords`, `passwd`, `pwd`, `passphrase`, `passphrases`, `passcode`, `privatekey`,
  `privatekeys`, `secretkey`, `accesskey`, `sshkey`, `encryptionkey`, `signingkey`, `apikey`, `apikeys`, `secret`,
  `token`, `tokens`, `credential`, `credentials`, `authorization`, `connectionstring` or `dsn`; or
- is exactly `pass`, `pw`, `key`, `pin`, `bearer` or `cookie`.

So `password`, `dbPassword`, `ssh_password`, `sshKeyPassphrase`, `tls_key_passphrase`, `private_key`, `clientSecret`,
`accessToken`, `iamToken`, `api-key`, `aws_secret_access_key`, `dbPwd` and `connectionString` are refused.

**A name and value pair**: an object with a `value` that is not `null`, `true` or `false` and a string `name` or `key`
that is a secret name by the rule above, such as `{"name": "password", "value": "..."}`. `field` is the path of the
`value`. In an object that has a `value`, the field `key` is the pair's name and is not itself treated as a secret.

**By what a string holds**, whatever its field is called:

- Private key material: a PEM private key block (`-----BEGIN PRIVATE KEY-----`, also with a type in front as in `RSA
  PRIVATE KEY`, `EC PRIVATE KEY`, `ENCRYPTED PRIVATE KEY`, `OPENSSH PRIVATE KEY`, and `PGP PRIVATE KEY BLOCK`) or a
  PuTTY key (`PuTTY-User-Key-File-<n>:`), anywhere in the string. Certificates, public keys and other PEM blocks are
  not secrets and are kept, so SQL that holds one syncs.
- A string that as a whole is an address or connection string with a password in it. Only a string that starts as one
  counts, so SQL that uses a URL or mentions a password is kept:
  - `scheme://user:password@...` (also `jdbc:scheme://...`), with or without a user before the colon;
  - `user:password@host`, optionally with `:port` and `/path`, as the whole string;
  - a URL with a parameter `password`, `passwd`, `pwd`, `pass`, `sslpassword`, `secret`, `token`, `access_token`,
    `api_key` or `apikey` that has a value;
  - a string made up only of two or more `key=value` pairs separated by spaces or semicolons (libpq, ODBC, ADO.NET)
    that has a `password`, `passwd`, `pwd`, `pass` or `sslpassword` with a value and one of `host`, `hostaddr`,
    `server`, `data source`, `addr`, `address`, `dbname`, `database`, `initial catalog`, `user`, `username`, `uid`,
    `user id` or `port`.
- JSON in a string: a string that starts with `{` or `[` (after any white space) and is valid JSON is read, and what
  it holds is checked by all of these rules. `field` is the path of the string.

Names that only talk about secrets are kept: `savedSecrets` (the desktop app's list of secret kinds a device keeps,
such as `["db_password"]`), `secretKind`, `passwordSaved`, `privateKeyPath`, `clientKeyPath`, `keyPath`, `sslmode`, and
`auth` (how the SSH tunnel signs in: `"password"`, `"key_file"` or `"agent"`). Every field of the desktop app's
records passes. Key and certificate files sync as paths, never as contents. A client that adds fields picks names
that these rules do not refuse.

## Limits

| What | Limit | Past it |
|---|---|---|
| Changes per push | 100 | 422, nothing stored |
| Body of a push | 8388608 bytes | 413, nothing stored |
| Payload | 262144 bytes | that change: `too_large` |
| Nesting in a payload | 32 levels below the payload | that change: `invalid` |
| Records per page | 1 to 1000 (default 200), and what fits in 4194304 bytes of payload | 422 for a `limit` out of range |
| Live records per organization | `VARVE_SYNC_MAX_RECORDS`, 50000 unless the server sets it | that change: `record_limit` |
| Live records per user feed | `VARVE_SYNC_MAX_USER_RECORDS`, 10000 unless the server sets it | that change: `record_limit` |
| Bytes of the payloads of the live records per organization | `VARVE_SYNC_MAX_BYTES`, 536870912 (512 MiB) unless the server sets it | that change: `storage_limit` |
| Bytes of the payloads of the live records per user feed | `VARVE_SYNC_MAX_USER_BYTES`, 67108864 (64 MiB) unless the server sets it | that change: `storage_limit` |
| Revisions kept per saved query | `VARVE_SYNC_MAX_REVISIONS`, 50 unless the server sets it, and `VARVE_SYNC_MAX_REVISION_BYTES`, 8388608 bytes (8 MiB) unless the server sets it | the oldest are dropped |
| Pushes | 300 per user and 600 per organization in any 10 minutes | 429 |
| Pulls (and revision reads) | 600 per user and 1200 per organization in any 10 minutes | 429 |
| Tombstones | kept 90 days after the delete | purged; older cursors get 410 |

The per-user limits count the requests of all of the user's devices and feeds together, also requests for an
organization that is not the user's; the per-organization limits count all of its members.

The size of a payload is counted in bytes of the JSON as the server stores it (see [Payloads](#payloads)). A feed that
is over its limit of bytes, because the server lowered it, keeps what it has: records can still be deleted, made
smaller or changed without growing.

## Worked example

Kim's laptop creates a folder with a connection in it, her desktop changes the connection while offline, and the
laptop deletes it.

1. The laptop pushes to its personal organization (`0196f3c2-0000-7000-8000-00000000000a`):

   ```
   POST /api/v1/sync/organizations/0196f3c2-0000-7000-8000-00000000000a/push
   ```
   ```json
   {"changes": [
     {"op": "upsert", "id": "0196f3c2-1111-7000-8000-000000000001", "kind": "folder", "baseVersion": 0,
      "payload": {"name": "Webshop", "parentId": null, "color": null, "sortOrder": 0}},
     {"op": "upsert", "id": "0196f3c2-2222-7000-8000-000000000002", "kind": "connection", "baseVersion": 0,
      "payload": {"name": "Shop production", "folderId": "0196f3c2-1111-7000-8000-000000000001", "engine": "postgres",
                  "host": "db.shop.example.com", "port": 5432, "database": "shop", "username": "kim",
                  "environment": "production", "tls": {"mode": "verify_full"}, "ssh": {"enabled": false},
                  "options": {"readOnly": true}, "savedSecrets": ["db_password"]}}
   ]}
   ```
   ```json
   {"results": [
     {"id": "0196f3c2-1111-7000-8000-000000000001", "status": "accepted", "version": 1},
     {"id": "0196f3c2-2222-7000-8000-000000000002", "status": "accepted", "version": 1}
   ]}
   ```

2. The desktop has never synced, so it pulls without a cursor and gets both records with `"hasMore": false` and a
   cursor, say `C1`.

3. The desktop renames the connection: an upsert with `"baseVersion": 1` and the whole new payload. Answer:
   `{"id": "0196f3c2-2222-7000-8000-000000000002", "status": "accepted", "version": 2}`.

4. The laptop, which still has version 1, deletes the connection:

   ```json
   {"changes": [{"op": "delete", "id": "0196f3c2-2222-7000-8000-000000000002", "baseVersion": 1}]}
   ```
   ```json
   {"results": [{"id": "0196f3c2-2222-7000-8000-000000000002", "status": "conflict",
     "current": {"id": "0196f3c2-2222-7000-8000-000000000002", "kind": "connection", "version": 2, "deleted": false,
                 "payload": {"name": "Shop production (EU)", "...": "..."}, "createdAt": "2026-11-12T13:00:00+00:00",
                 "createdBy": "0196f3c2-0000-7000-8000-0000000000ff", "updatedAt": "2026-11-12T13:05:00+00:00",
                 "updatedBy": "0196f3c2-0000-7000-8000-0000000000ff",
                 "updatedByDevice": "0196f3c2-0000-7000-8000-0000000000d2"}}]}
   ```

   The laptop takes version 2, shows Kim what changed, and when she still wants it gone pushes
   `{"op": "delete", "id": "0196f3c2-2222-7000-8000-000000000002", "baseVersion": 2}`, which is accepted with
   `"version": 3`.

5. The desktop pulls with `C1`:

   ```
   GET /api/v1/sync/organizations/0196f3c2-0000-7000-8000-00000000000a/changes?cursor=C1
   ```
   ```json
   {"changes": [{"id": "0196f3c2-2222-7000-8000-000000000002", "kind": "connection", "version": 3, "deleted": true,
                 "payload": null, "createdAt": "2026-11-12T13:00:00+00:00",
                 "createdBy": "0196f3c2-0000-7000-8000-0000000000ff", "updatedAt": "2026-11-12T13:07:00+00:00",
                 "updatedBy": "0196f3c2-0000-7000-8000-0000000000ff",
                 "updatedByDevice": "0196f3c2-0000-7000-8000-0000000000d1"}],
    "cursor": "C2", "hasMore": false}
   ```

   It deletes its copy of the connection (and the keychain entries under its id) and stores `C2`.
