# The usage ping's Worker

The other end of `crates/orchd-serve/src/ping.rs`. A Cloudflare Worker named
`orchd-ping` on the owner's account, writing one row per install per UTC day to
the D1 database `orchd-pings`.

Deploy after a change, from this directory:

```
npx wrangler deploy
npx wrangler d1 execute orchd-pings --remote --file schema.sql   # only for a new database
```

## Reading it

```
npx wrangler d1 execute orchd-pings --remote --command "<sql>"
```

Daily active installs, the last 30 days:

```sql
SELECT day, COUNT(*) AS installs FROM pings
WHERE day >= date('now', '-30 days') GROUP BY day ORDER BY day;
```

Weekly active installs, and what they run:

```sql
SELECT version, os, arch, COUNT(DISTINCT id) AS installs FROM pings
WHERE day >= date('now', '-7 days') GROUP BY version, os, arch ORDER BY installs DESC;
```

An install is a config dir, not a person: one person on two machines is two, and
a deleted `ping.json` is a new install.
