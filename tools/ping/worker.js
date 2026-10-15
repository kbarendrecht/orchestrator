// The other end of the app's daily usage ping (`crates/orchd-serve/src/ping.rs`).
//
// One row per install per UTC day: the install id, the version and the platform.
// **Not the IP**, which Cloudflare hands every request and this deliberately does
// not write — the id already answers "how many", and an address would answer
// "who", which nobody asked.
//
// D1 rather than Analytics Engine because Analytics Engine needs enabling by hand
// in the dashboard, and D1 answers the same questions exactly with plain SQL.
// `schema.sql` is the table; the queries worth running are in the README.
//
// Deployed by hand (`npx wrangler deploy` here); nothing in CI ships it, because
// nothing in this repo should hold a Cloudflare token.

const ID = /^[0-9a-f]{32}$/
// Bounded so a malformed client cannot store whatever it likes.
const field = (v, max) => (typeof v === 'string' ? v.slice(0, max) : '')

export default {
  async fetch(req, env) {
    if (req.method !== 'POST' || new URL(req.url).pathname !== '/ping') {
      return new Response(null, { status: 404 })
    }
    /* **Per address, before anything touches the database.** Without it anyone
       could post fresh ids to inflate the count or spend D1's daily write quota,
       after which real pings fail. A real install sends one ping a day and retries
       hourly, so ten a minute leaves room for a whole office behind one NAT. The
       address is the limiter's key in memory and is still not stored.

       **It is loose, by Cloudflare's design and measured so:** the counter lives on
       each server and settles in the background. Eighty requests one after another
       in a minute all passed; a parallel burst of 150 had 27 refused. So it stops a
       flood, not a patient script — enough for a count nobody is paid on. */
    const { success } = await env.LIMITER.limit({ key: req.headers.get('cf-connecting-ip') ?? '' })
    if (!success) return new Response(null, { status: 429 })
    let body
    try {
      body = await req.json()
    } catch {
      return new Response(null, { status: 400 })
    }
    // A string first: `test` coerces, so `["<32 hex>"]` passed and then made
    // `bind` throw, which answered 500 rather than refusing.
    if (typeof body?.id !== 'string' || !ID.test(body.id)) {
      return new Response(null, { status: 400 })
    }
    // The day is the server's, so a client clock that is wrong cannot invent one.
    // A second ping the same day keeps the row and takes its version: an upgrade
    // mid-day is then counted on the build it ended the day on.
    await env.DB.prepare(
      `INSERT INTO pings (id, day, version, os, arch) VALUES (?1, date('now'), ?2, ?3, ?4)
       ON CONFLICT (id, day) DO UPDATE SET version = excluded.version`,
    )
      .bind(body.id, field(body.version, 32), field(body.os, 16), field(body.arch, 16))
      .run()
    return new Response(null, { status: 204 })
  },
}
