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
| `createdAt`, `updatedAt` | Server time of the create and of the last write, RFC 3339 with offset, to the second. |
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
    { "id": "<uuid>", "status": "conflict", "current": { "id": "<uuid>", "kind": "query", "version": 5, "deleted": false, "payload": {}, "createdAt": "...", "updatedAt": "...", "updatedBy": "...", "updatedByDevice": "..." } },
    { "id": "<uuid>", "status": "rejected", "reason": "secret", "field": "payload.ssh.password", "message": "payload.ssh.password looks like a secret. Passwords, passphrases, keys and tokens never leave the device." }
  ]
}
```

| Status | Fields | Meaning |
|---|---|---|
| `accepted` | `id`, `version` | Stored; `version` is the record's new version. The record also has a new place in the feed, so the next pull returns it (the client's own write comes back to it). |
| `conflict` | `id`, `current` | `baseVersion` is not the record's version. `current` is the server's copy in the [record shape](#records), a tombstone included, or `null` when the server has no record with this id (it never had one, or its tombstone was purged). Nothing was written. |
| `rejected` | `id`, `reason`, `field`, `message` | The server will not store this change, whatever the client retries. `id` is `null` when the change had no valid id. `field` is the path of the cause inside the change (`id`, `op`, `baseVersion`, `kind`, `payload`, `payload.tls.mode`, `payload.tags[2]`), or `null`. `message` is for people. |

Reasons of a rejection:

| `reason` | When |
|---|---|
| `invalid` | The change or its payload breaks a rule of the envelope or of its kind; also an upsert that would change the kind of an existing record (`field` is `kind`). |
| `unknown_kind` | `kind` is not one of the feed's kinds. |
| `secret` | The payload holds a secret (see [Secrets](#secrets)). |
| `too_large` | The payload is larger than 262144 bytes. |
| `not_found` | A delete of a record the server does not have. |
| `record_limit` | The upsert would create or bring back a record while the feed is at its limit of live records. |

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
- Every accepted upsert or delete of a `query` keeps the payload it replaced as a revision (the last 50).

### Errors of the whole request

These store nothing. Bodies are `application/problem+json`.

| Status | When | Body |
|---|---|---|
| 400 | The body is not JSON. | `{"title": "An error occurred", "detail": "The body is not valid JSON.", "status": 400, "type": "/errors/400"}` |
| 401 | No token, or the token or its device is no longer valid. | not JSON |
| 403 | The token has no `sync` scope. | problem JSON |
| 404 | Not the signed-in user's personal organization. | `{"title": "An error occurred", "detail": "Organization not found.", "status": 404, "type": "/errors/404"}` |
| 413 | The body is larger than 8388608 bytes (8 MiB). Split the changes over more pushes. | `{"title": "An error occurred", "detail": "A push can be at most 8388608 bytes; send the changes in more than one push.", "status": 413, "type": "/errors/413"}` |
| 415 | The body is not sent as `application/json`. | problem JSON |
| 422 | The body is not an object with a list `changes`, or the list has more than 100 changes. | `{"status": 422, "violations": [{"propertyPath": "changes", "message": "A push can hold at most 100 changes.", "code": null}], "detail": "changes: A push can hold at most 100 changes.", "type": "/validation_errors/...", "title": "An error occurred"}` |
| 429 | Too many pushes (see [Limits](#limits)). Wait the seconds in `Retry-After`. | `{"title": "An error occurred", "detail": "Too many sync requests. Try again later.", "status": 429, "type": "/errors/429"}` |

A development server adds a `trace` to problem bodies; clients ignore unknown fields.

## Pull

```
GET /api/v1/sync/organizations/{organizationId}/changes?cursor=<cursor>&limit=<n>
GET /api/v1/sync/user/changes?cursor=<cursor>&limit=<n>
```

| Parameter | Rules |
|---|---|
| `cursor` | The `cursor` of the previous answer for this feed. Leave it out for a first pull. |
| `limit` | Records per page at most, 1 to 1000; 200 when left out. A page also ends once its payloads pass 4 MiB, but always holds at least one record. |

Answer, `200 OK`:

```json
{
  "changes": [ { "id": "<uuid>", "kind": "folder", "version": 2, "deleted": false, "payload": { "name": "Webshop" }, "createdAt": "...", "updatedAt": "...", "updatedBy": "...", "updatedByDevice": "..." } ],
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

Errors:

| Status | When | Body |
|---|---|---|
| 401, 403, 404, 429 | As for a push. | |
| 410 | The cursor is too old: tombstones after it were purged (90 days after their delete), or it is from a state the server does not have (a server restored from an older backup). Pull everything again without a cursor and replace the local copy of the feed with it, keeping local changes that are not pushed yet. | `{"title": "An error occurred", "detail": "This cursor is too old: deletes after it were purged. Pull everything again without a cursor.", "status": 410, "type": "/errors/410"}` |
| 422 | `cursor` is not a cursor of this feed, or `limit` is out of range. | `{"status": 422, "violations": [{"propertyPath": "cursor", "message": "This is not a cursor of these records.", "code": null}], ...}` |

## Revisions

```
GET /api/v1/sync/organizations/{organizationId}/records/{recordId}/revisions
```

`200 OK` with the payloads a saved query had before, newest first, the last 50:

```json
{
  "revisions": [
    { "version": 4, "payload": { "name": "Open orders", "sql": "SELECT 1" }, "updatedAt": "2026-11-12T14:30:00+00:00", "updatedBy": "<user id or null>", "updatedByDevice": "<device id or null>" }
  ]
}
```

`version` is the version that payload had; the record itself holds the current one. A record of another kind has an
empty list; a record the organization does not have (or no longer has, after its tombstone was purged) is a 404. To
restore a revision, push it as an upsert on the current version. Counts as a pull for the limits.

## Payloads

Every payload is a JSON object of at most 262144 bytes (256 KiB) as the server stores it: compact JSON with slashes and
non-ASCII characters unescaped. Objects and lists nest at most 32 levels below the payload. Numbers must lie within the
signed 64-bit integer range, -2^63 to 2^63 - 1; any other number is rejected, since it would not come back as it was
sent.

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
keychain. A payload that holds one is rejected with reason `secret` and the path of the field:

- A field whose name, after dropping everything but letters and digits and lower-casing, is `pwd` or ends in
  `password`, `passwd`, `passphrase`, `privatekey`, `secret`, `token` or `apikey`, at any depth, unless its value is
  `null`, `true` or `false`. So `password`, `dbPassword`, `ssh_password`, `sshKeyPassphrase`, `tls_key_passphrase`,
  `private_key`, `clientSecret`, `accessToken`, `iamToken` and `api-key` are refused.
- Any string value holding a PEM block (`-----BEGIN ` + upper-case letters, digits and spaces + `-----`, which covers
  private keys, OpenSSH keys and certificates) or a PuTTY key (`PuTTY-User-Key-File-<n>:`), whatever its field.

Names that only talk about secrets are kept: `savedSecrets` (the desktop app's list of secret kinds a device keeps,
such as `["db_password"]`), `secretKind`, `passwordSaved`, `privateKeyPath`, `clientKeyPath`, `keyPath`, `sslmode`.
Key and certificate files sync as paths, never as contents.

## Limits

| What | Limit |
|---|---|
| Changes per push | 100 |
| Body of a push | 8388608 bytes |
| Payload | 262144 bytes |
| Records per page | 1 to 1000 (default 200), and about 4 MiB of payload |
| Live records per organization | `VARVE_SYNC_MAX_RECORDS`, 50000 unless the server sets it |
| Live records per user feed | `VARVE_SYNC_MAX_USER_RECORDS`, 10000 unless the server sets it |
| Pushes | 300 per user and 600 per organization in any 10 minutes |
| Pulls (and revision reads) | 600 per user and 1200 per organization in any 10 minutes |
| Tombstones | kept 90 days after the delete |

The per-user limits count the requests of all of the user's devices and feeds together; the per-organization limits
count all of its members.

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
                 "updatedAt": "2026-11-12T13:05:00+00:00", "updatedBy": "0196f3c2-0000-7000-8000-0000000000ff",
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
                 "payload": null, "createdAt": "2026-11-12T13:00:00+00:00", "updatedAt": "2026-11-12T13:07:00+00:00",
                 "updatedBy": "0196f3c2-0000-7000-8000-0000000000ff",
                 "updatedByDevice": "0196f3c2-0000-7000-8000-0000000000d1"}],
    "cursor": "C2", "hasMore": false}
   ```

   It deletes its copy of the connection (and the keychain entries under its id) and stores `C2`.
