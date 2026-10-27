# Varve patch to tokio-postgres 0.7.18

This directory is `tokio-postgres` 0.7.18 exactly as published on crates.io
(MIT or Apache-2.0, see `LICENSE-MIT` and `LICENSE-APACHE`), with the small
change described here. The root `Cargo.toml` points cargo at it:

```toml
[patch.crates-io]
tokio-postgres = { path = "vendor/tokio-postgres" }
```

The only user is `crates/driver-postgres`, which pins
`tokio-postgres = "=0.7.18"`.

## Why

A PostgreSQL user can create objects in a schema that comes before
`pg_catalog` in the `search_path` (for example `SET search_path = public,
pg_catalog`, or a role or database default). An unqualified operator in a
query then resolves to the user's object when its argument types match
exactly: a `public.=(oid, oid)` operator wins over `pg_catalog.=(oid, oid)`.

Varve qualifies every name in the SQL it sends itself. tokio-postgres also
sends queries of its own: when a prepared statement returns or takes a type
it does not know (an enum, a composite, a domain, a range or any extension
type), it looks the type up in the system catalogs with the queries in
`src/prepare.rs`. Those used unqualified `=` and `>` operators and an
unqualified `OID` cast, so a planted operator would run (with the session
user's rights, inside the user's transaction, from a query the user did not
write) every time a column of such a type is read.

## Changes against upstream 0.7.18

Only `src/prepare.rs` changes, and only the texts of the five type lookup
queries (`TYPEINFO_QUERY`, `TYPEINFO_FALLBACK_QUERY`, `TYPEINFO_ENUM_QUERY`,
`TYPEINFO_ENUM_FALLBACK_QUERY`, `TYPEINFO_COMPOSITE_QUERY`), each marked with
a `VARVE-PATCH` comment:

- `a = b` becomes `a OPERATOR(pg_catalog.=) b` (join conditions and the
  `$1` parameter comparison);
- `attnum > 0` becomes `attnum OPERATOR(pg_catalog.>) 0`;
- `NULL::OID` becomes `NULL::pg_catalog.oid`.

Relations were already qualified (`pg_catalog.pg_type` and so on), column
names cannot be hijacked, and `ORDER BY` uses the column type's default
sort operator from its operator class, not a `search_path` lookup. The
parameter `$1` is still inferred as `oid` from the qualified operator. The
result columns and their order are unchanged, so nothing else in the crate
is affected.

`crates/driver-postgres/src/tap.rs` (`TYPEINFO_QUERIES`) recognises these
statements by their exact text, to know which prepared statements are
tokio-postgres's own; it holds the patched texts and must change with them.
`crates/driver-postgres/tests/hijack.rs` plants such operators and checks
that type lookups never run them.

## Updating

To move to another tokio-postgres version, copy the new release here, apply
the same change to its type lookup queries, update `TYPEINFO_QUERIES`, the
pinned version in `crates/driver-postgres/Cargo.toml` and this file, and run
the driver's integration tests (including `tests/hijack.rs`).
