# Travel

**Development preview · source-available under FSL-1.1-ALv2.** The complete
product plan is not finished. This repository is shared for inspection,
experimentation, and contributions—not as a production-ready travel service.
See [security guidance](SECURITY.md) and [third-party notices](THIRD_PARTY_NOTICES.md).

An independent travel organizer for a household. It runs without Envelope,
a central account, telemetry, or a model provider. Rust serves the bundled
Svelte interface and keeps records in SQLite. The optional macOS companion
synchronizes app-managed calendar events and reminders.

This is a development build, not a validated release of the entire product
scope. See the platform notes below before relying on alerts while traveling.

## Run locally

From this repository, with Rust, Node.js 22+, and npm installed:

```sh
cd crates/dashboard/web
npm ci
npm run build
cd ../../..
cargo run --locked -p travel -- serve
```

Open `http://127.0.0.1:3150/travel`. A port conflict produces a diagnostic;
use `serve --port 3151` to choose another port. `travel doctor` prints the
resolved database path without printing credentials.

For an isolated instance, set `TRAVEL_HOME` to a directory you control.
Data lives under `TRAVEL_HOME/travel`; this does not read Envelope's database.
Without this override, the platform configuration directory is used.

The service is unauthenticated on loopback by default. Set a strong random
`TRAVEL_TOKEN` before exposing it through a proxy, tunnel, or network interface.
Non-loopback binding requires it. Sign in using that token on the Connections
page. Never put the token in a URL. HTTPS is required for remote use.

For private remote and family access, follow the [Tailscale setup](deploy/tailscale.md).
It keeps Travel on loopback behind Tailscale Serve HTTPS, retains household
authentication, and does not use public Funnel exposure.

## Receipts and intelligence

You can start without a mailbox: use the receipt importer or Connections file
upload. `.eml`, UTF-8 text, and text-bearing PDFs are supported; PDF extraction
requires `pdftotext` (Poppler). Scans, encrypted files, and unsupported files
need review. File and mailbox originals are content-addressed locally.

Mailbox access is read-only. Gmail app passwords and generic TLS IMAP use the
connection API; the current onboarding form is Gmail-oriented. Generic IMAP
does not trust sender-supplied authentication headers for amendments.

Intelligence is disabled until configured in Connections. An API connection
uses an OpenAI-compatible chat-completions endpoint and a server environment
variable containing its key. A CLI adapter receives JSON on standard input
and returns JSON on standard output. Only explicitly opted-in unsandboxed CLI
execution is currently implemented; use a trusted wrapper, not arbitrary
receipt-provided commands. Timeouts, output limits, and daily call limits apply.
Receipt content is disclosed to the selected provider; credentials are not
included in that content.

Validated declarative parsers are stored with fixtures and history. Matching
demonstrated layouts can be extracted without another model call; booking
identity still requires review for learned formats. Owners can list parser
versions at `/api/v1/parsers` and restore a version with
`POST /api/v1/parsers/{id}/restore`. Generated executable/WASM parsers are not
enabled in this build.

## Family, calendars, and notifications

Owners create trip-scoped editor/viewer invitations in Connections. Invitation
links are single-use; browser sessions expire after 30 days and revocation
invalidates them. Read-only public shares and separately scoped calendar
tokens remain separate from household accounts.

Connections offers a private calendar download with action URLs and alarms.
Treat private calendar files as sensitive. Family feeds redact reservation
credentials. No external reservation link is prefetched or executed.

Browser push is optional, off by default, and requires explicit browser
permission and acceptance of the browser vendor's transport. Its messages are
generic; a durable queue tracks provider acceptance, retries, and expiry.
Provider acceptance is not proof the user saw a notification. Use HTTPS for
remote push; localhost can be used for desktop development. iPhone requires
a Home Screen installation. Real-device push acceptance testing is pending.

## macOS companion

On macOS 14+ with Swift build tools:

```sh
bash scripts/package-macos.sh
open dist/Travel.app
```

The package includes the local service. A fresh output path is required; the
script never replaces an existing package. It signs ad hoc by default and does
not notarize. Set `TRAVEL_SIGNING_IDENTITY` to your identity for a signed build.
Public distribution still requires your Apple signing/notarization workflow.

The bundle includes a locally rendered app icon. On first local launch, the
companion generates an owner token in Keychain and passes it to the bundled
service. Use “Copy token for browser sign-in” to sign in on Connections; the
clipboard is cleared after 60 seconds if unchanged. Do not share the owner token
with family members—create scoped invitations instead. Quit other Travel copies
before opening the installed app so port 3150 belongs to the intended instance.

Calendar and Reminders permissions are requested only when enabled. The current
companion uses writable local destinations; it does not silently select iCloud.
If none exists, use calendar downloads. Native notifications and explicit
synced-destination selection remain to be completed. Native edits are preserved
as conflicts; completion is synchronized separately. Actual EventKit permission
and real-calendar acceptance tests have not yet been performed.

## Linux and dedicated hosted instances

Build with `cargo build --locked --release -p travel` after building browser
assets. Install the binary as `/usr/local/bin/travel`. The supplied systemd unit
expects a dedicated `travel` user and an owner-only `/etc/travel/environment`
containing `TRAVEL_TOKEN` and `TRAVEL_MASTER_KEY`. Generate and retain independent
high-entropy values; losing the master key can make credentials unrecoverable.
Do not commit these files. Each household gets its own process, volume, tokens,
master key, and hostname. There is no shared-account service.

Alternatively, build the container from the repository root:

```sh
docker build -f deploy/Dockerfile -t travel:local .
```

Mount a persistent `/data` volume writable by UID 10001 and supply credentials
through your chosen secret manager or protected environment file. Bind the
published port to loopback behind your HTTPS proxy. The example Caddyfile is
optional; enabling automatic certificates contacts its certificate authority.
The container and systemd recipes still require Linux deployment validation.

Laptop operation catches up while awake. Previously exported calendar alarms
can remain useful, but a sleeping host cannot provide timely new alerts or
refresh family feeds. Live flight lookups, full multi-source ingestion, complete
editing flows, automatic repair/reprocessing, and encrypted credential-transfer
bundles are not yet release-complete. Do not rely on this build as your only
source of travel deadlines.

## Backup and restore

`travel backup --output /path/to/new-backup-directory` takes a consistent SQLite
snapshot and copies immutable originals plus the intelligence configuration.
It verifies document hashes and writes a completion manifest last. Treat the
backup as private: it contains receipts, itinerary data, and private action links.
Never put credentials inline in model arguments; use environment references.

Credential fields, OAuth grants, push devices, sessions, memberships, and share
capabilities are removed from the copied database, not from your live database.
Google OAuth client configuration and the credential store are excluded. Keep
your own protected copy of provider configuration if needed.

Restore into a fresh data location:

```sh
TRAVEL_HOME=/path/to/new-parent travel restore --snapshot /path/to/backup-directory
TRAVEL_HOME=/path/to/new-parent travel serve
```

The parent directory must exist and its `travel` subdirectory must not exist.
Restore verifies the manifest and database integrity before writing. It never
overwrites an existing instance. Reconnect mailboxes and reissue household/share
links after restoration. Stable itinerary and calendar IDs are retained.

## Development checks

```sh
cargo test -p envelope-email-dashboard
cd crates/dashboard/web
npm run check
npm run build
```

Existing Envelope data is never automatically migrated. The snapshot importer
previews with `travel import-envelope --snapshot /path/to/snapshot.db`; add
`--apply` only after reviewing its counts. Use a database snapshot, not Envelope's
live database. Sharing links must be reissued for the new origin.
