<script lang="ts">
  import { onMount } from 'svelte';
  import BookingEditor from '$lib/BookingEditor.svelte';
  import { EnvelopeApiError as TravelApiError } from '$lib/api';
  import {
    buildReceiptApprovalInput,
    isTravelAlertDue,
    summarizeReceiptImport,
    summarizeTravelSync,
    travelApi,
    type Booking,
    type Receipt,
    type ReceiptApprovalDraft,
    type Segment,
    type TravelAlert,
    type TravelOverview,
    type TravelShare,
    type TravelTask,
    type Trip
  } from '$lib/travel-api';

  type View = 'overview' | 'receipts' | 'coordinate' | 'settings';

  let overview = $state<TravelOverview | null>(null);
  let loading = $state(true);
  let pageError = $state<string | null>(null);
  let notice = $state<string | null>(null);
  let view = $state<View>('overview');
  let busy = $state<string | null>(null);

  // Gmail onboarding. The app password exists only in this in-memory field
  // while the form is open and is cleared before the network request begins.
  let gmailEmail = $state('');
  let appPassword = $state('');
  let onboardingTimezone = $state(Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC');
  let onboardingScanFolder = $state('INBOX');

  let importText = $state('');
  let receiptEdits = $state<Record<string, ReceiptApprovalDraft>>({});
  let tripTitle = $state('');
  let tripDestination = $state('');
  let tripStartsAt = $state('');
  let tripEndsAt = $state('');
  let tripTimezone = $state(Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC');
  let tripNotes = $state('');

  let bookingTripId = $state('');
  let bookingKind = $state('flight');
  let bookingTitle = $state('');
  let bookingProvider = $state('');
  let bookingStartsAt = $state('');
  let bookingEndsAt = $state('');
  let bookingLocation = $state('');

  let taskTripId = $state('');
  let taskTitle = $state('');
  let taskDueAt = $state('');

  let shareTripId = $state('');
  let shareLabel = $state('Family');
  let shareCanEditTasks = $state(true);
  let shareExpiresAt = $state('');
  let newestShare = $state<{ url: string; calendarUrl: string } | null>(null);
  let copied = $state<string | null>(null);

  let settingsTimezone = $state('UTC');
  let settingsCalendarName = $state('Travel');
  let settingsAutoIngest = $state(true);
  let settingsScanFolder = $state('INBOX');
  let recentSyncAt = $state<string | null>(null);
  let alertClock = $state(Date.now());

  const trips = $derived(safeList<Trip>(overview?.trips));
  const receipts = $derived(safeList<Receipt>(overview?.receipts));
  const tasks = $derived(safeList<TravelTask>(overview?.tasks));
  const alerts = $derived(safeList<TravelAlert>(overview?.alerts));
  const shares = $derived(safeList<TravelShare>(overview?.shares ?? overview?.share_links));
  const bookings = $derived(safeList<Booking>(overview?.bookings));
  const segments = $derived(safeList<Segment>(overview?.segments));
  const selectedAccount = $derived(
    overview?.accounts?.find((account) => account.id === overview?.settings?.account_id) ??
      overview?.account ??
      null
  );
  const gmailOnboardingSecure = $derived(overview?.gmail_onboarding_secure !== false);
  let useWithoutMailbox = $state(false);
  const connected = $derived(Boolean(overview?.settings?.account_id ?? overview?.account?.id));
  const openTasks = $derived(tasks.filter((task) => !taskCompleted(task)));
  const activeAlerts = $derived(alerts.filter((alert) => isTravelAlertDue(alert, alertClock)));
  const trueSyncAt = $derived(
    recentSyncAt ??
      overview?.sync?.last_synced_at ??
      overview?.sync?.last_sync_at ??
      overview?.settings?.last_synced_at ??
      null
  );
  const pendingReceipts = $derived(
    receipts.filter((receipt) =>
      ['pending', 'review', 'needs_review', 'quarantined'].includes(receiptStatus(receipt))
    )
  );

  onMount(() => {
    void load();
    const timer = window.setInterval(() => (alertClock = Date.now()), 30_000);
    return () => window.clearInterval(timer);
  });

  function safeList<T>(value: unknown): T[] {
    return Array.isArray(value) ? (value as T[]) : [];
  }

  function message(error: unknown, fallback: string): string {
    if (error instanceof TravelApiError || error instanceof Error) return error.message;
    return fallback;
  }

  async function load({ quiet = false }: { quiet?: boolean } = {}) {
    if (!quiet) loading = true;
    pageError = null;
    try {
      overview = await travelApi.overview();
      if (overview.trips.length > 0 || localStorage.getItem('travel-without-mailbox') === 'true') useWithoutMailbox = true;
      seedReceiptEdits(overview.receipts);
      const zone = overview.settings?.timezone ?? overview.settings?.home_timezone;
      if (zone) {
        settingsTimezone = zone;
        tripTimezone = zone;
      }
      if (overview.settings?.calendar_name) settingsCalendarName = overview.settings.calendar_name;
      if (overview.settings?.scan_folder) {
        settingsScanFolder = overview.settings.scan_folder;
        onboardingScanFolder = overview.settings.scan_folder;
      }
      if (typeof overview.settings?.auto_ingest === 'boolean') {
        settingsAutoIngest = overview.settings.auto_ingest;
      }
      if (!bookingTripId && trips[0]) bookingTripId = trips[0].id;
      if (!taskTripId && trips[0]) taskTripId = trips[0].id;
      if (!shareTripId && trips[0]) shareTripId = trips[0].id;
    } catch (error) {
      pageError = message(error, 'Travel could not be loaded.');
    } finally {
      loading = false;
    }
  }

  async function run<T>(key: string, success: string, action: () => Promise<T>) {
    busy = key;
    notice = null;
    pageError = null;
    try {
      const result = await action();
      notice = success;
      await load({ quiet: true });
      return result;
    } catch (error) {
      pageError = message(error, 'That action could not be completed.');
      return undefined;
    } finally {
      busy = null;
    }
  }

  async function connectGmail(event: SubmitEvent) {
    event.preventDefault();
    if (!gmailOnboardingSecure) {
      appPassword = '';
      pageError =
        'Gmail setup is disabled on this connection. Open Travel on localhost or through HTTPS/Tailscale Serve.';
      return;
    }
    const email = gmailEmail.trim();
    const password = appPassword;
    appPassword = '';
    const result = await run('connect', 'Mailbox connected. Your travel inbox is ready.', () =>
      travelApi.connectGmail({
        email,
        password,
        display_name: 'Travel mailbox',
        settings: {
          timezone: onboardingTimezone,
          home_timezone: onboardingTimezone,
          scan_folder: onboardingScanFolder.trim() || 'INBOX',
          calendar_name: 'Travel',
          auto_ingest: true
        }
      }).then((connectedMailbox) => {
        if (!connectedMailbox.settings) {
          throw new Error(
            connectedMailbox.verification.error ||
              'Gmail rejected this app password. Create a fresh app password and try again.'
          );
        }
        return connectedMailbox;
      })
    );
    if (result) void syncMailbox();
  }

  async function syncMailbox() {
    const accountId = overview?.settings?.account_id ?? overview?.account?.id;
    if (!accountId) {
      pageError = 'Connect a travel mailbox before checking for receipts.';
      return;
    }
    busy = 'sync';
    notice = null;
    pageError = null;
    try {
      const result = await travelApi.sync({
        account_id: accountId,
        folder: overview?.settings?.scan_folder || settingsScanFolder || undefined
      });
      if (result.last_sync_at) recentSyncAt = result.last_sync_at;
      await load({ quiet: true });
      const summary = summarizeTravelSync(result);
      if (summary.tone === 'warning') pageError = summary.message;
      else notice = summary.message;
    } catch (error) {
      pageError = message(error, 'The mailbox check failed before it returned a result.');
    } finally {
      busy = null;
    }
  }

  async function importReceipt(event: SubmitEvent) {
    event.preventDefault();
    const content = importText.trim();
    if (!content) return;
    busy = 'import';
    notice = null;
    pageError = null;
    try {
      const result = await travelApi.importReceipt({
        account_id: overview?.settings?.account_id ?? undefined,
        from_addr: 'manual-import@travel.local',
        subject: content.split(/\r?\n/, 1)[0]?.slice(0, 160) || 'Manually imported travel receipt',
        body_text: content
      });
      await load({ quiet: true });
      const summary = summarizeReceiptImport(result);
      if (summary.tone === 'warning') pageError = summary.message;
      else {
        notice = summary.message;
        importText = '';
      }
    } catch (error) {
      pageError = message(error, 'The receipt import failed before it returned a result.');
    } finally {
      busy = null;
    }
  }

  async function approveReceipt(event: SubmitEvent, receipt: Receipt) {
    event.preventDefault();
    const draft = receiptEdits[receipt.id];
    if (!draft) return;
    await run(`receipt-${receipt.id}`, 'Receipt approved and added to the itinerary.', () =>
      travelApi.approveReceipt(receipt.id, buildReceiptApprovalInput(draft))
    );
  }

  async function dismissReceipt(receipt: Receipt) {
    await run(`receipt-${receipt.id}`, 'Receipt dismissed.', () =>
      travelApi.dismissReceipt(receipt.id)
    );
  }

  async function createTrip(event: SubmitEvent) {
    event.preventDefault();
    const result = await run('trip', 'Trip added.', () =>
      travelApi.createTrip({
        title: tripTitle.trim(),
        destination: tripDestination.trim() || undefined,
        starts_at: localIso(tripStartsAt),
        ends_at: localIso(tripEndsAt),
        timezone: tripTimezone,
        notes: tripNotes.trim() || undefined
      })
    );
    if (result) {
      tripTitle = '';
      tripDestination = '';
      tripStartsAt = '';
      tripEndsAt = '';
      tripNotes = '';
    }
  }

  async function createBooking(event: SubmitEvent) {
    event.preventDefault();
    const result = await run('booking', 'Booking added to the itinerary.', () =>
      travelApi.createBooking({
        trip_id: bookingTripId,
        kind: bookingKind,
        title: bookingTitle.trim(),
        provider: bookingProvider.trim() || undefined,
        starts_at: localIso(bookingStartsAt),
        ends_at: localIso(bookingEndsAt),
        location: bookingLocation.trim() || undefined
      })
    );
    if (result) {
      bookingTitle = '';
      bookingProvider = '';
      bookingStartsAt = '';
      bookingEndsAt = '';
      bookingLocation = '';
    }
  }

  async function addTask(event: SubmitEvent) {
    event.preventDefault();
    const result = await run('task', 'Coordination task added.', () =>
      travelApi.createTask(taskTripId, {
        title: taskTitle.trim(),
        due_at: localIso(taskDueAt)
      })
    );
    if (result) {
      taskTitle = '';
      taskDueAt = '';
    }
  }

  async function toggleTask(task: TravelTask) {
    await run(`task-${task.id}`, taskCompleted(task) ? 'Task reopened.' : 'Task completed.', () =>
      travelApi.toggleTask(task.id)
    );
  }

  async function acknowledgeAlert(alert: TravelAlert) {
    await run(`alert-${alert.id}`, 'Alert acknowledged.', () =>
      travelApi.acknowledgeAlert(alert.id)
    );
  }

  async function createShare(event: SubmitEvent) {
    event.preventDefault();
    const result = await run('share', 'Private family link created.', () =>
      travelApi.createShare({
        trip_id: shareTripId,
        label: shareLabel.trim() || undefined,
        can_edit_tasks: shareCanEditTasks,
        expires_at: localIso(shareExpiresAt)
      })
    );
    if (result) {
      const raw = result as Record<string, unknown>;
      const token = typeof raw.token === 'string' ? raw.token : '';
      const suppliedUrl =
        typeof raw.url === 'string'
          ? raw.url
          : typeof raw.share_url === 'string'
            ? raw.share_url
            : '';
      const relativeUrl = suppliedUrl || (token ? `/share/${encodeURIComponent(token)}` : '');
      const relativeCalendarUrl =
        typeof raw.calendar_url === 'string'
          ? raw.calendar_url
          : token
            ? `/api/public/travel/${encodeURIComponent(token)}/calendar.ics`
            : '';
      const url = relativeUrl ? new URL(relativeUrl, window.location.origin).toString() : '';
      const calendarUrl = relativeCalendarUrl
        ? new URL(relativeCalendarUrl, window.location.origin).toString()
        : '';
      newestShare = url ? { url, calendarUrl } : null;
    }
  }

  async function revokeShare(share: TravelShare) {
    await run(`share-${share.id}`, 'Family link revoked.', () => travelApi.deleteShare(share.id));
  }

  async function saveSettings(event: SubmitEvent) {
    event.preventDefault();
    await run('settings', 'Travel preferences saved.', () =>
      travelApi.saveSettings({
        timezone: settingsTimezone,
        home_timezone: settingsTimezone,
        scan_folder: settingsScanFolder.trim() || 'INBOX',
        calendar_name: settingsCalendarName.trim() || 'Travel',
        auto_ingest: settingsAutoIngest
      })
    );
  }

  async function copyText(value: string, key: string) {
    try {
      await navigator.clipboard.writeText(value);
      copied = key;
      window.setTimeout(() => {
        if (copied === key) copied = null;
      }, 1800);
    } catch {
      pageError = 'Copy was blocked by the browser. Select the link and copy it manually.';
    }
  }

  function localIso(value: string): string | undefined {
    if (!value) return undefined;
    const parsed = new Date(value);
    return Number.isNaN(parsed.getTime()) ? value : parsed.toISOString();
  }

  function tripStart(trip: Trip): string | undefined {
    return trip.start_at ?? trip.starts_at ?? undefined;
  }

  function tripEnd(trip: Trip): string | undefined {
    return trip.end_at ?? trip.ends_at ?? undefined;
  }

  function bookingStart(booking: Booking): string | undefined {
    return booking.start_at ?? booking.starts_at ?? undefined;
  }

  function segmentStart(segment: Segment): string | undefined {
    return segment.start_at ?? segment.departs_at ?? undefined;
  }

  function segmentEnd(segment: Segment): string | undefined {
    return segment.end_at ?? segment.arrives_at ?? undefined;
  }

  function formatDate(value?: string | null, style: 'short' | 'long' | 'time' = 'short'): string {
    if (!value) return 'Not set';
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return value;
    if (style === 'time') {
      return new Intl.DateTimeFormat(undefined, {
        month: 'short',
        day: 'numeric',
        hour: 'numeric',
        minute: '2-digit'
      }).format(parsed);
    }
    return new Intl.DateTimeFormat(undefined, {
      month: style === 'long' ? 'long' : 'short',
      day: 'numeric',
      year: style === 'long' ? 'numeric' : undefined
    }).format(parsed);
  }

  function dateRange(trip: Trip): string {
    const start = tripStart(trip);
    const end = tripEnd(trip);
    if (!start && !end) return 'Dates not set';
    if (!end || start === end) return formatDate(start);
    return `${formatDate(start)} — ${formatDate(end)}`;
  }

  function receiptStatus(receipt: Receipt): string {
    return String(receipt.status ?? receipt.decision ?? 'pending').toLowerCase();
  }

  function seedReceiptEdits(nextReceipts: Receipt[] | undefined) {
    if (!Array.isArray(nextReceipts)) return;
    for (const receipt of nextReceipts) {
      if (receiptEdits[receipt.id]) continue;
      receiptEdits[receipt.id] = {
        kind: receipt.kind && !['unknown', 'receipt'].includes(receipt.kind) ? receipt.kind : '',
        title: receipt.title || receipt.subject || '',
        provider: receipt.provider || '',
        confirmation_code: '',
        status: receipt.booking_status || receipt.parsed_status || 'confirmed',
        start_at: toDateTimeInput(receipt.start_at),
        end_at: toDateTimeInput(receipt.end_at),
        origin: receipt.origin || '',
        destination: receipt.destination || '',
        clear_provider: false,
        clear_confirmation_code: false,
        clear_end_at: false,
        clear_origin: false,
        clear_destination: false
      };
    }
  }

  function toDateTimeInput(value?: string | null): string {
    if (!value) return '';
    const parsed = new Date(value);
    if (Number.isNaN(parsed.getTime())) return value.slice(0, 16);
    const local = new Date(parsed.getTime() - parsed.getTimezoneOffset() * 60_000);
    return local.toISOString().slice(0, 16);
  }

  function taskCompleted(task: TravelTask): boolean {
    return Boolean(task.completed ?? task.completed_at);
  }

  function tripName(id?: string | null): string {
    if (!id) return 'General travel';
    return trips.find((trip) => trip.id === id)?.title ?? 'Trip';
  }

  function tripBookings(trip: Trip): Booking[] {
    const nested = safeList<Booking>(trip.bookings);
    return nested.length > 0 ? nested : bookings.filter((booking) => booking.trip_id === trip.id);
  }

  function bookingSegments(booking: Booking): Segment[] {
    const nested = safeList<Segment>(booking.segments);
    return nested.length > 0
      ? nested
      : segments.filter((segment) => segment.booking_id === booking.id);
  }

  function tripTasks(trip: Trip): TravelTask[] {
    const nested = safeList<TravelTask>(trip.tasks);
    return nested.length > 0 ? nested : tasks.filter((task) => task.trip_id === trip.id);
  }

  function sortedTrips(): Trip[] {
    return [...trips].sort((a, b) => {
      const left = tripStart(a) ? Date.parse(tripStart(a)!) : Number.MAX_SAFE_INTEGER;
      const right = tripStart(b) ? Date.parse(tripStart(b)!) : Number.MAX_SAFE_INTEGER;
      return left - right;
    });
  }
</script>

<svelte:head>
  <title>Travel · Travel</title>
  <meta
    name="description"
    content="A private travel inbox, itinerary, receipt review desk, and family calendar."
  />
</svelte:head>

<div class="travel-page">
  <header class="travel-header">
    <div>
      <p class="eyebrow">Travel travel desk</p>
      <h1>Every trip, from inbox to arrival.</h1>
      <p class="lede">Receipts become a calm itinerary, with one shared plan for everyone going.</p>
    </div>
    <div class="header-actions">
      {#if connected}
        <span class="connection-pill"><span class="status-dot"></span>Mailbox connected</span>
        <button class="button ghost" disabled={busy === 'sync'} onclick={syncMailbox}>
          {busy === 'sync' ? 'Checking…' : 'Check for receipts'}
        </button>
      {/if}
    </div>
  </header>

  {#if pageError}
    <div class="flash error" role="alert">
      <span>{pageError}</span>
      <button aria-label="Dismiss error" onclick={() => (pageError = null)}>×</button>
    </div>
  {/if}
  {#if notice}
    <div class="flash success" role="status">
      <span>{notice}</span>
      <button aria-label="Dismiss notification" onclick={() => (notice = null)}>×</button>
    </div>
  {/if}

  {#if loading}
    <div class="loading-card" aria-live="polite">
      <span class="loader" aria-hidden="true"></span>
      Opening your travel desk…
    </div>
  {:else if !overview && pageError}
    <section class="empty-load panel">
      <p class="eyebrow">Travel is taking a detour</p>
      <h2>We couldn’t open this workspace.</h2>
      <p>The rest of Travel is untouched. Retry when the service is available.</p>
      <button class="button primary" onclick={() => load()}>Try again</button>
    </section>
  {:else if !connected && !useWithoutMailbox}
    <section class="onboarding-grid" aria-labelledby="connect-title">
      <div class="onboarding-story">
        <span class="step-number">01</span>
        <h2 id="connect-title">Give travel receipts their own runway.</h2>
        <button class="button primary" onclick={() => { useWithoutMailbox = true; localStorage.setItem('travel-without-mailbox','true'); }}>Use without a mailbox</button>
        <p>
          Connect the Gmail address where airlines, hotels, trains, and rental companies send
          confirmations. Travel reads the mailbox from this computer or your private server.
        </p>
        <ol class="promise-list">
          <li><span>01</span> App password goes directly to your local Travel credential store.</li>
          <li><span>02</span> Mail is checked over Gmail IMAP; verification does not send email.</li>
          <li><span>03</span> Your family sees only the trip you explicitly share.</li>
        </ol>
      </div>

      {#if gmailOnboardingSecure}
      <form class="connect-card" onsubmit={connectGmail} aria-describedby="connect-privacy">
        <div class="card-heading">
          <p class="eyebrow">Private connection</p>
          <h2>Connect Gmail</h2>
        </div>
        <label>
          <span>Gmail address</span>
          <input
            type="email"
            bind:value={gmailEmail}
            autocomplete="email"
            placeholder="you@gmail.com"
            required
          />
        </label>
        <label>
          <span>16-character app password</span>
          <input
            type="password"
            bind:value={appPassword}
            autocomplete="new-password"
            minlength="16"
            placeholder="xxxx xxxx xxxx xxxx"
            required
          />
          <small>
            Use a Google app password, not your everyday password.
            <a href="https://support.google.com/accounts/answer/185833" target="_blank" rel="noreferrer">
              How to create one ↗
            </a>
          </small>
        </label>
        <label>
          <span>Home timezone</span>
          <input bind:value={onboardingTimezone} autocomplete="off" required />
        </label>
        <label>
          <span>Folder to scan</span>
          <input bind:value={onboardingScanFolder} autocomplete="off" required />
          <small>
            Enter the exact Gmail folder name. Use <code>INBOX</code> for new mail or
            <code>[Gmail]/All Mail</code> to include archived receipts.
          </small>
        </label>
        <button class="button primary wide" type="submit" disabled={busy === 'connect'}>
          {busy === 'connect' ? 'Connecting securely…' : 'Connect travel mailbox'}
        </button>
        <p class="privacy-note" id="connect-privacy">
          The app password is never saved in this browser. “Connected” confirms IMAP access only;
          Travel does not send a test message.
        </p>
      </form>
      {:else}
        <section class="connect-card blocked-card" aria-labelledby="secure-setup-title">
          <div class="card-heading">
            <p class="eyebrow">Secure connection required</p>
            <h2 id="secure-setup-title">Gmail setup is unavailable here.</h2>
          </div>
          <p>
            An app password must never cross a plain remote connection. Open Travel directly on
            <code>localhost</code>, or publish it through HTTPS or Tailscale Serve, then return to
            Travel to connect Gmail.
          </p>
          <p class="privacy-note">
            No Gmail address or app-password field is available until the connection is protected.
          </p>
        </section>
      {/if}
    </section>
  {:else}
    <nav class="view-tabs" aria-label="Travel sections">
      <button class:active={view === 'overview'} onclick={() => (view = 'overview')}>
        Overview <span>{trips.length}</span>
      </button>
      <button class:active={view === 'receipts'} onclick={() => (view = 'receipts')}>
        Receipts <span>{pendingReceipts.length}</span>
      </button>
      <button class:active={view === 'coordinate'} onclick={() => (view = 'coordinate')}>
        Coordinate <span>{openTasks.length + activeAlerts.length}</span>
      </button>
      <button class:active={view === 'settings'} onclick={() => (view = 'settings')}>Settings</button>
    </nav>

    {#if view === 'overview'}
      <section class="summary-strip" aria-label="Travel summary">
        <div><strong>{trips.length}</strong><span>Trips</span></div>
        <div><strong>{bookings.length || trips.reduce((n, trip) => n + tripBookings(trip).length, 0)}</strong><span>Bookings</span></div>
        <div><strong>{pendingReceipts.length}</strong><span>Receipts to review</span></div>
        <div><strong>{openTasks.length}</strong><span>Open tasks</span></div>
      </section>

      <div class="workspace-grid">
        <main class="itinerary-column">
          <div class="section-heading">
            <div>
              <p class="eyebrow">Chronological itinerary</p>
              <h2>Where you’re going</h2>
            </div>
            <span class="quiet">Times shown in your browser timezone</span>
          </div>

          {#if trips.length === 0}
            <div class="empty-state panel">
              <span class="empty-mark">→</span>
              <h3>No trips yet</h3>
              <p>Check your mailbox for receipts, paste a confirmation, or add the first trip.</p>
              <button class="text-button" onclick={() => (view = 'receipts')}>Import a receipt →</button>
            </div>
          {:else}
            <div class="trip-list">
              {#each sortedTrips() as trip (trip.id)}
                <article class="trip-card">
                  <div class="trip-spine" aria-hidden="true"><span></span></div>
                  <div class="trip-body">
                    <header class="trip-card-head">
                      <div>
                        <p class="trip-date">{dateRange(trip)}</p>
                        <h3>{trip.title}</h3>
                        <p class="destination">{trip.destination || 'Destination to be decided'}</p>
                      </div>
                      <span class="status {trip.status || 'planning'}">{trip.status || 'planning'}</span>
                    </header>

                    {#if trip.notes}<p class="trip-notes">{trip.notes}</p>{/if}

                    {#if tripBookings(trip).length > 0}
                      <ol class="booking-timeline">
                        {#each tripBookings(trip) as booking (booking.id)}
                          <li>
                            <span class="kind-mark" aria-hidden="true">{bookingKindMark(booking.kind)}</span>
                            <div class="booking-main">
                              <div class="booking-line">
                                <strong>{booking.title || booking.provider || titleCase(booking.kind)}</strong>
                                <time>{formatDate(bookingStart(booking), 'time')}</time>
                              </div>
                              <p>
                                {[booking.provider, booking.location].filter(Boolean).join(' · ') || titleCase(booking.kind)}
                              </p>
                              {#if booking.confirmation_masked}
                                <span class="confirmation">Confirmation {booking.confirmation_masked}</span>
                              {/if}
                              <BookingEditor {booking} refreshed={()=>load({quiet:true})}/>
                              {#if bookingSegments(booking).length > 0}
                                <div class="segments">
                                  {#each bookingSegments(booking) as segment (segment.id)}
                                    <div class="segment">
                                      <span>{segment.title || titleCase(segment.kind)}</span>
                                      <span>{segment.origin || segment.location || '—'} → {segment.destination || '—'}</span>
                                      <time>{formatDate(segmentStart(segment), 'time')} — {formatDate(segmentEnd(segment), 'time')}</time>
                                    </div>
                                  {/each}
                                </div>
                              {/if}
                            </div>
                          </li>
                        {/each}
                      </ol>
                    {:else}
                      <p class="empty-inline">No bookings attached yet. Mail sync will add matching confirmations.</p>
                    {/if}

                    {#if tripTasks(trip).length > 0}
                      <div class="trip-task-summary">
                        {tripTasks(trip).filter((task) => !taskCompleted(task)).length} coordination tasks open
                      </div>
                    {/if}
                  </div>
                </article>
              {/each}
            </div>
          {/if}
        </main>

        <aside class="quick-column">
          <details class="panel composer-panel" open={trips.length === 0}>
            <summary>Add a trip <span>＋</span></summary>
            <form class="compact-form" onsubmit={createTrip}>
              <label><span>Trip name</span><input bind:value={tripTitle} placeholder="Autumn in Kyoto" required /></label>
              <label><span>Destination</span><input bind:value={tripDestination} placeholder="Kyoto, Japan" /></label>
              <div class="two-fields">
                <label><span>Starts</span><input type="datetime-local" bind:value={tripStartsAt} /></label>
                <label><span>Ends</span><input type="datetime-local" bind:value={tripEndsAt} /></label>
              </div>
              <label><span>Timezone</span><input bind:value={tripTimezone} required /></label>
              <label><span>Notes</span><textarea rows="3" bind:value={tripNotes} placeholder="What everyone should know"></textarea></label>
              <button class="button primary wide" type="submit" disabled={busy === 'trip'}>{busy === 'trip' ? 'Adding…' : 'Add trip'}</button>
            </form>
          </details>

          {#if trips.length > 0}
            <details class="panel composer-panel">
              <summary>Add a booking <span>＋</span></summary>
              <form class="compact-form" onsubmit={createBooking}>
                <label><span>Trip</span><select bind:value={bookingTripId} required>{#each trips as trip}<option value={trip.id}>{trip.title}</option>{/each}</select></label>
                <div class="two-fields">
                  <label><span>Type</span><select bind:value={bookingKind}><option value="flight">Flight</option><option value="hotel">Hotel</option><option value="train">Train</option><option value="car">Car</option><option value="activity">Activity</option><option value="other">Other</option></select></label>
                  <label><span>Provider</span><input bind:value={bookingProvider} placeholder="Air France" /></label>
                </div>
                <label><span>Booking title</span><input bind:value={bookingTitle} placeholder="Paris to Tokyo" required /></label>
                <label><span>Location</span><input bind:value={bookingLocation} placeholder="CDG Terminal 2E" /></label>
                <div class="two-fields">
                  <label><span>Starts</span><input type="datetime-local" bind:value={bookingStartsAt} /></label>
                  <label><span>Ends</span><input type="datetime-local" bind:value={bookingEndsAt} /></label>
                </div>
                <button class="button primary wide" type="submit" disabled={busy === 'booking'}>{busy === 'booking' ? 'Adding…' : 'Add booking'}</button>
              </form>
            </details>
          {/if}

          <section class="panel next-panel">
            <div class="panel-title-line"><h3>Needs attention</h3><button class="text-button" onclick={() => (view = 'coordinate')}>Open desk</button></div>
            {#if activeAlerts.length === 0 && openTasks.length === 0}
              <p class="empty-inline">Nothing urgent. You’re clear for takeoff.</p>
            {:else}
              {#each activeAlerts.slice(0, 2) as alert (alert.id)}
                <div class="attention-item alert-item"><span>{alert.severity || 'info'}</span><strong>{alert.title}</strong></div>
              {/each}
              {#each openTasks.slice(0, 3) as task (task.id)}
                <div class="attention-item"><span>task</span><strong>{task.title}</strong></div>
              {/each}
            {/if}
          </section>
        </aside>
      </div>
    {:else if view === 'receipts'}
      <div class="receipts-layout">
        <main>
          <div class="section-heading">
            <div><p class="eyebrow">Inbox intelligence</p><h2>Receipt review</h2></div>
            <button class="button ghost" disabled={busy === 'sync'} onclick={syncMailbox}>{busy === 'sync' ? 'Checking…' : 'Check mailbox'}</button>
          </div>
          {#if receipts.length === 0}
            <div class="empty-state panel"><span class="empty-mark">@</span><h3>No travel receipts found</h3><p>Check Gmail or paste a receipt beside this list. Other email remains outside the travel desk.</p></div>
          {:else}
            <div class="receipt-list">
              {#each receipts as receipt (receipt.id)}
                <article class="receipt-card">
                  <div class="receipt-meta">
                    <span class="status {receiptStatus(receipt)}">{receiptStatus(receipt).replaceAll('_', ' ')}</span>
                    {#if typeof receipt.confidence === 'number'}<span>{Math.round(receipt.confidence * 100)}% confidence</span>{/if}
                    <span>{receipt.kind || 'travel'}</span>
                  </div>
                  <h3>{receipt.subject || 'Untitled travel receipt'}</h3>
                  <p class="sender">{receipt.sender || receipt.from_addr || 'Unknown sender'} · {formatDate(receipt.received_at, 'time')}</p>
                  {#if receipt.excerpt || receipt.body_text}<p class="excerpt">{receipt.excerpt || receipt.body_text}</p>{/if}
                  {#if receipt.trip_id}<p class="receipt-trip">Filed with {tripName(receipt.trip_id)}</p>{/if}
                  {#if receipt.quarantine_reason}<p class="quarantine">Held for review: {receipt.quarantine_reason}</p>{/if}
                  {#if ['pending', 'review', 'needs_review', 'quarantined'].includes(receiptStatus(receipt))}
                    {@const edit = receiptEdits[receipt.id]}
                    {#if edit}
                      <form class="receipt-review-form" onsubmit={(event) => approveReceipt(event, receipt)}>
                        <p class="review-label">Correct the extracted details before approval</p>
                        <div class="receipt-fields">
                          <label>
                            <span>Type</span>
                            <select bind:value={edit.kind} required>
                              <option value="" disabled>Choose type</option>
                              <option value="flight">Flight</option>
                              <option value="hotel">Hotel</option>
                              <option value="train">Train</option>
                              <option value="car">Car rental</option>
                              <option value="cruise">Cruise</option>
                              <option value="activity">Activity</option>
                              <option value="other">Other</option>
                            </select>
                          </label>
                          <label class="span-two"><span>Title</span><input bind:value={edit.title} required /></label>
                          <div class="receipt-field">
                            <label><span>Provider</span><input bind:value={edit.provider} disabled={edit.clear_provider} /></label>
                            {#if receipt.provider}
                              <label class="clear-value"><input type="checkbox" bind:checked={edit.clear_provider} /><span>Clear extracted provider</span></label>
                            {/if}
                          </div>
                          <div class="receipt-field">
                            <label>
                              <span>Confirmation code</span>
                              <input
                                bind:value={edit.confirmation_code}
                                autocomplete="off"
                                disabled={edit.clear_confirmation_code}
                                placeholder={receipt.confirmation_masked || 'Enter only to correct'}
                              />
                              {#if receipt.confirmation_masked}<small>Extracted {receipt.confirmation_masked}. Leave blank to keep it.</small>{/if}
                            </label>
                            {#if receipt.confirmation_masked}
                              <label class="clear-value"><input type="checkbox" bind:checked={edit.clear_confirmation_code} /><span>Clear extracted confirmation</span></label>
                            {/if}
                          </div>
                          <label>
                            <span>Booking status</span>
                            <select bind:value={edit.status} required>
                              <option value="confirmed">Confirmed</option>
                              <option value="changed">Changed</option>
                              <option value="cancelled">Cancelled</option>
                              <option value="pending">Pending</option>
                            </select>
                          </label>
                          <label><span>Starts</span><input type="datetime-local" bind:value={edit.start_at} /></label>
                          <div class="receipt-field">
                            <label><span>Ends</span><input type="datetime-local" bind:value={edit.end_at} disabled={edit.clear_end_at} /></label>
                            {#if receipt.end_at}
                              <label class="clear-value"><input type="checkbox" bind:checked={edit.clear_end_at} /><span>Clear extracted end time</span></label>
                            {/if}
                          </div>
                          <div class="receipt-field">
                            <label><span>Origin</span><input bind:value={edit.origin} disabled={edit.clear_origin} /></label>
                            {#if receipt.origin}
                              <label class="clear-value"><input type="checkbox" bind:checked={edit.clear_origin} /><span>Clear extracted origin</span></label>
                            {/if}
                          </div>
                          <div class="receipt-field">
                            <label><span>Destination</span><input bind:value={edit.destination} disabled={edit.clear_destination} /></label>
                            {#if receipt.destination}
                              <label class="clear-value"><input type="checkbox" bind:checked={edit.clear_destination} /><span>Clear extracted destination</span></label>
                            {/if}
                          </div>
                        </div>
                        <div class="receipt-actions">
                          <button class="button primary" type="submit" disabled={busy === `receipt-${receipt.id}`}>Approve corrected details</button>
                          <button class="button ghost" type="button" disabled={busy === `receipt-${receipt.id}`} onclick={() => dismissReceipt(receipt)}>Dismiss receipt</button>
                        </div>
                      </form>
                    {/if}
                  {/if}
                </article>
              {/each}
            </div>
          {/if}
        </main>
        <aside>
          <form class="panel paste-card" onsubmit={importReceipt}>
            <p class="eyebrow">Forward without forwarding</p>
            <h2>Paste a confirmation</h2>
            <p>Drop in the text from any receipt. Travel will extract the itinerary and leave uncertain details for review.</p>
            <label>
              <span>Receipt or confirmation text</span>
              <textarea rows="13" bind:value={importText} placeholder="Paste airline, hotel, train, car, or activity details…" required></textarea>
            </label>
            <button class="button primary wide" type="submit" disabled={busy === 'import'}>{busy === 'import' ? 'Reading receipt…' : 'Import receipt'}</button>
            <small>Original text stays with the review record for traceability.</small>
          </form>
        </aside>
      </div>
    {:else if view === 'coordinate'}
      <div class="coordinate-grid">
        <section class="panel coordinate-panel">
          <div class="section-heading compact"><div><p class="eyebrow">Timely signals</p><h2>Alerts</h2></div><span class="count-badge">{activeAlerts.length} active</span></div>
          {#if activeAlerts.length === 0}
            <p class="empty-inline padded">No alerts are due. Future reminders will appear here at their scheduled time.</p>
          {:else}
            <div class="action-list">
              {#each activeAlerts as alert (alert.id)}
                <article class="action-row alert-row">
                  <span class="severity {alert.severity || 'info'}">{alert.severity || 'info'}</span>
                  <div><strong>{alert.title}</strong><p>{alert.body || tripName(alert.trip_id)}</p>{#if alert.scheduled_at}<time>{formatDate(alert.scheduled_at, 'time')}</time>{/if}</div>
                  <button class="button ghost" disabled={busy === `alert-${alert.id}`} onclick={() => acknowledgeAlert(alert)}>Acknowledge</button>
                </article>
              {/each}
            </div>
          {/if}
        </section>

        <section class="panel coordinate-panel">
          <div class="section-heading compact"><div><p class="eyebrow">Shared checklist</p><h2>Tasks</h2></div><span class="count-badge">{openTasks.length} open</span></div>
          {#if tasks.length === 0}<p class="empty-inline padded">No tasks yet. Add the first coordination detail below.</p>{/if}
          <div class="task-list">
            {#each tasks as task (task.id)}
              <label class:done={taskCompleted(task)} class="task-row">
                <input type="checkbox" checked={taskCompleted(task)} disabled={busy === `task-${task.id}`} onchange={() => toggleTask(task)} />
                <span><strong>{task.title}</strong><small>{tripName(task.trip_id)}{task.due_at ? ` · due ${formatDate(task.due_at, 'time')}` : ''}</small></span>
              </label>
            {/each}
          </div>
          {#if trips.length > 0}
            <form class="inline-task-form" onsubmit={addTask}>
              <label><span>Task</span><input bind:value={taskTitle} placeholder="Check everyone in" required /></label>
              <label><span>Trip</span><select bind:value={taskTripId}>{#each trips as trip}<option value={trip.id}>{trip.title}</option>{/each}</select></label>
              <label><span>Due</span><input type="datetime-local" bind:value={taskDueAt} /></label>
              <button class="button primary" type="submit" disabled={busy === 'task'}>{busy === 'task' ? 'Adding…' : 'Add task'}</button>
            </form>
          {/if}
        </section>

        <section class="panel sharing-panel">
          <div class="section-heading compact"><div><p class="eyebrow">Private coordination</p><h2>Family sharing</h2></div></div>
          <p class="section-copy">Create an unguessable link for one trip. Family can follow the itinerary and optionally check off shared tasks—without seeing confirmation numbers.</p>
          {#if trips.length === 0}
            <p class="empty-inline">Add a trip before creating a family link.</p>
          {:else}
            <form class="share-form" onsubmit={createShare}>
              <label><span>Trip</span><select bind:value={shareTripId} required>{#each trips as trip}<option value={trip.id}>{trip.title}</option>{/each}</select></label>
              <label><span>Link label</span><input bind:value={shareLabel} placeholder="Family" /></label>
              <label><span>Expires (optional)</span><input type="datetime-local" bind:value={shareExpiresAt} /></label>
              <label class="check-label"><input type="checkbox" bind:checked={shareCanEditTasks} /><span>Let family check off shared tasks</span></label>
              <button class="button primary" type="submit" disabled={busy === 'share'}>{busy === 'share' ? 'Creating…' : 'Create private link'}</button>
            </form>
          {/if}

          {#if newestShare}
            <div class="new-share" role="status">
              <strong>Save these links now</strong>
              <p>For privacy, the full token may only be shown once.</p>
              <div class="copy-row"><input readonly value={newestShare.url} aria-label="New family share link" /><button class="button ghost" onclick={() => copyText(newestShare!.url, 'share')}>{copied === 'share' ? 'Copied' : 'Copy'}</button></div>
              {#if newestShare.calendarUrl}<div class="copy-row"><input readonly value={newestShare.calendarUrl} aria-label="New shared calendar link" /><button class="button ghost" onclick={() => copyText(newestShare!.calendarUrl, 'calendar')}>{copied === 'calendar' ? 'Copied' : 'Copy calendar'}</button></div>{/if}
            </div>
          {/if}

          {#if shares.length > 0}
            <div class="share-list">
              {#each shares as share (share.id)}
                <div class="share-row">
                  <div><strong>{share.label || 'Family link'}</strong><p>{tripName(share.trip_id)} · token …{share.token_prefix || 'private'}{share.can_edit_tasks ? ' · tasks enabled' : ''}</p></div>
                  <button class="button danger" disabled={busy === `share-${share.id}`} onclick={() => revokeShare(share)}>Revoke</button>
                </div>
              {/each}
            </div>
          {/if}
        </section>
      </div>
    {:else}
      <div class="settings-grid">
        <form class="panel settings-card" onsubmit={saveSettings}>
          <p class="eyebrow">Travel preferences</p>
          <h2>Mailbox & calendar</h2>
          <label><span>Home timezone</span><input bind:value={settingsTimezone} required /></label>
          <label>
            <span>Folder to scan</span>
            <input bind:value={settingsScanFolder} autocomplete="off" required />
            <small>
              Exact Gmail name, such as <code>INBOX</code> or <code>[Gmail]/All Mail</code>.
              Travel reads it without marking messages as read.
            </small>
          </label>
          <label><span>Calendar name</span><input bind:value={settingsCalendarName} required /></label>
          <label class="check-label"><input type="checkbox" bind:checked={settingsAutoIngest} /><span>Automatically inspect new mail for travel receipts</span></label>
          <button class="button primary" type="submit" disabled={busy === 'settings'}>{busy === 'settings' ? 'Saving…' : 'Save preferences'}</button>
        </form>
        <section class="panel security-card">
          <p class="eyebrow">Connection health</p>
          <h2>Mailbox connected</h2>
          <dl>
            <div><dt>Account</dt><dd>{selectedAccount?.username || overview?.settings?.account_email || 'Gmail'}</dd></div>
            <div><dt>Incoming mail</dt><dd>imap.gmail.com · encrypted</dd></div>
            <div><dt>Receipt folder</dt><dd>{overview?.settings?.scan_folder || 'INBOX'}</dd></div>
            {#if trueSyncAt}
              <div><dt>Last mailbox check</dt><dd>{formatDate(trueSyncAt, 'time')}</dd></div>
            {/if}
            <div><dt>View refreshed</dt><dd>{formatDate(overview?.generated_at, 'time')}</dd></div>
          </dl>
          <p class="privacy-note">The app password is held by Travel’s credential store on the machine running the service. It is never returned to this page.</p>
          <button class="button ghost" disabled={busy === 'sync'} onclick={syncMailbox}>{busy === 'sync' ? 'Checking…' : 'Verify by checking mail'}</button>
        </section>
      </div>
    {/if}
  {/if}
</div>

<script lang="ts" module>
  function titleCase(value?: string | null): string {
    if (!value) return 'Travel';
    return value.replaceAll('_', ' ').replace(/\b\w/g, (letter) => letter.toUpperCase());
  }

  function bookingKindMark(kind?: string | null): string {
    switch (String(kind).toLowerCase()) {
      case 'flight': return 'FL';
      case 'hotel': return 'HT';
      case 'train': return 'TR';
      case 'car': return 'CR';
      case 'activity': return 'GO';
      default: return '•';
    }
  }
</script>

<style>
  .travel-page { width: 100%; min-height: 100%; overflow: auto; padding: clamp(1rem, 3vw, 2.5rem); background: var(--env-page); }
  .travel-header { max-width: 1440px; margin: 0 auto 1.5rem; display: flex; align-items: flex-end; justify-content: space-between; gap: 2rem; }
  h1, h2, h3, p { margin-top: 0; }
  h1 { max-width: 720px; margin-bottom: .5rem; font-size: clamp(2rem, 4vw, 3.75rem); line-height: .98; letter-spacing: -.045em; font-weight: 600; }
  h2 { margin-bottom: .4rem; font-size: clamp(1.35rem, 2vw, 2rem); line-height: 1.05; letter-spacing: -.025em; }
  h3 { margin-bottom: .25rem; font-size: 1.05rem; }
  .eyebrow { margin-bottom: .55rem; color: var(--env-accent); font-family: var(--font-mono); font-size: .68rem; font-weight: 500; letter-spacing: .16em; text-transform: uppercase; }
  .lede { max-width: 620px; margin-bottom: 0; color: #5f5c56; font-size: 1.05rem; }
  .header-actions { display: flex; align-items: center; gap: .65rem; flex-wrap: wrap; justify-content: flex-end; }
  .connection-pill { min-height: 44px; display: inline-flex; align-items: center; gap: .5rem; padding: 0 .85rem; background: var(--env-accent-soft); border: 1px solid #b8dccc; color: var(--env-accent); font-family: var(--font-mono); font-size: .72rem; text-transform: uppercase; }
  .status-dot { width: 7px; height: 7px; border-radius: 50%; background: var(--env-accent); box-shadow: 0 0 0 3px rgba(26,107,74,.12); }
  .button { min-height: 44px; display: inline-flex; align-items: center; justify-content: center; border: 1px solid transparent; border-radius: 2px; padding: .65rem .95rem; font-weight: 600; font-size: .83rem; cursor: pointer; transition: 120ms ease; }
  .button:disabled { opacity: .48; cursor: wait; }
  .button.primary { background: var(--env-ink); color: white; }
  .button.primary:hover:not(:disabled) { background: var(--env-accent); }
  .button.ghost { background: var(--env-surface); border-color: var(--env-rule); color: var(--env-ink); }
  .button.ghost:hover:not(:disabled) { border-color: var(--env-accent); color: var(--env-accent); }
  .button.danger { background: transparent; color: var(--env-warn); border-color: #e9b5a4; }
  .wide { width: 100%; }
  .text-button { min-height: 44px; border: 0; padding: .4rem 0; background: transparent; color: var(--env-accent); font-weight: 600; cursor: pointer; }
  .flash { max-width: 1440px; min-height: 46px; margin: 0 auto 1rem; padding: .7rem .9rem; display: flex; align-items: center; justify-content: space-between; gap: 1rem; border: 1px solid; font-size: .88rem; }
  .flash.error { border-color: #e7b29f; color: #8f2f12; background: var(--env-warn-soft); }
  .flash.success { border-color: #b8dccc; color: var(--env-accent); background: var(--env-accent-soft); }
  .flash button { min-width: 44px; min-height: 44px; margin: -.65rem -.75rem -.65rem 0; border: 0; background: transparent; color: inherit; font-size: 1.25rem; cursor: pointer; }
  .loading-card, .empty-load { max-width: 1440px; margin: 2rem auto; }
  .loading-card { min-height: 300px; display: flex; align-items: center; justify-content: center; gap: .75rem; color: var(--env-muted); }
  .loader { width: 16px; height: 16px; border: 2px solid var(--env-rule); border-top-color: var(--env-accent); border-radius: 50%; animation: spin .7s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  .panel { background: var(--env-surface); border: 1px solid var(--env-rule); }
  .empty-load { padding: 3rem; text-align: center; }
  .onboarding-grid { max-width: 1200px; margin: 3rem auto; display: grid; grid-template-columns: minmax(0,1fr) minmax(340px,500px); gap: clamp(2rem,7vw,7rem); align-items: center; }
  .onboarding-story { position: relative; }
  .onboarding-story .step-number { position: absolute; top: -5rem; left: -1rem; color: #e1ded6; font-family: var(--font-mono); font-size: 7rem; font-weight: 300; z-index: 0; }
  .onboarding-story h2, .onboarding-story > p, .promise-list { position: relative; z-index: 1; }
  .onboarding-story h2 { max-width: 600px; font-size: clamp(2rem, 4vw, 3.4rem); }
  .onboarding-story > p { max-width: 590px; color: #5f5c56; line-height: 1.65; }
  .promise-list { list-style: none; margin: 2rem 0 0; padding: 0; border-top: 1px solid var(--env-rule); }
  .promise-list li { display: grid; grid-template-columns: 2rem 1fr; gap: 1rem; padding: 1rem 0; border-bottom: 1px solid var(--env-rule); line-height: 1.5; }
  .promise-list li span { color: var(--env-accent); font-family: var(--font-mono); font-size: .75rem; }
  .connect-card { padding: clamp(1.25rem, 4vw, 2.25rem); background: var(--env-surface); border: 1px solid var(--env-ink); box-shadow: 10px 10px 0 #dedbd3; }
  .blocked-card { border-color: var(--env-warn); box-shadow: 10px 10px 0 #ecd9d1; }
  .blocked-card > p:not(.eyebrow) { color: #5f5c56; line-height: 1.6; }
  .blocked-card code, form small code { font-family: var(--font-mono); font-size: .92em; }
  .card-heading { margin-bottom: 1.5rem; }
  form label { display: grid; gap: .4rem; margin-bottom: 1rem; }
  form label > span { font-size: .78rem; font-weight: 600; }
  input, select, textarea { width: 100%; min-height: 44px; border: 1px solid #c9c6bf; border-radius: 2px; padding: .68rem .75rem; background: #fff; color: var(--env-ink); }
  textarea { min-height: 90px; line-height: 1.5; resize: vertical; }
  label small, form > small { color: var(--env-muted); line-height: 1.45; }
  label small a { color: var(--env-accent); }
  .privacy-note { margin: 1rem 0 0; color: var(--env-muted); font-size: .75rem; line-height: 1.5; }
  .view-tabs { max-width: 1440px; margin: 0 auto 1.25rem; display: flex; border-bottom: 1px solid var(--env-rule); overflow-x: auto; }
  .view-tabs button { min-width: max-content; min-height: 48px; display: inline-flex; align-items: center; gap: .5rem; border: 0; border-bottom: 2px solid transparent; padding: 0 1.1rem; background: transparent; color: var(--env-muted); font-weight: 600; cursor: pointer; }
  .view-tabs button.active { border-bottom-color: var(--env-accent); color: var(--env-ink); }
  .view-tabs span { min-width: 20px; padding: .1rem .3rem; background: #e8e5de; color: #65625d; font-family: var(--font-mono); font-size: .67rem; }
  .summary-strip { max-width: 1440px; margin: 0 auto 1.25rem; display: grid; grid-template-columns: repeat(4,1fr); background: var(--env-ink); color: white; }
  .summary-strip div { min-height: 95px; display: flex; flex-direction: column; justify-content: center; padding: 1rem 1.25rem; border-right: 1px solid #393939; }
  .summary-strip div:last-child { border: 0; }
  .summary-strip strong { font-family: var(--font-mono); font-size: 1.75rem; font-weight: 400; }
  .summary-strip span { color: #aeaaa3; font-size: .72rem; text-transform: uppercase; letter-spacing: .08em; }
  .workspace-grid, .receipts-layout, .settings-grid { max-width: 1440px; margin: auto; display: grid; grid-template-columns: minmax(0,1fr) 360px; gap: 1.25rem; align-items: start; }
  .section-heading { min-height: 60px; margin-bottom: .8rem; display: flex; align-items: flex-end; justify-content: space-between; gap: 1rem; }
  .section-heading h2 { margin-bottom: 0; }
  .quiet { color: var(--env-muted); font-family: var(--font-mono); font-size: .67rem; }
  .trip-list { display: grid; gap: 1rem; }
  .trip-card { display: grid; grid-template-columns: 32px minmax(0,1fr); background: var(--env-surface); border: 1px solid var(--env-rule); }
  .trip-spine { display: flex; justify-content: center; background: var(--env-soft); border-right: 1px solid var(--env-rule); }
  .trip-spine::before { content: ''; width: 1px; height: 100%; background: var(--env-rule); }
  .trip-spine span { position: absolute; width: 9px; height: 9px; margin-top: 2.1rem; border: 2px solid var(--env-accent); background: white; transform: rotate(45deg); }
  .trip-body { padding: 1.4rem; min-width: 0; }
  .trip-card-head { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; }
  .trip-card-head h3 { font-size: 1.4rem; }
  .trip-date { margin-bottom: .25rem; color: var(--env-accent); font-family: var(--font-mono); font-size: .72rem; text-transform: uppercase; }
  .destination, .sender { margin-bottom: 0; color: var(--env-muted); font-size: .84rem; }
  .status { width: fit-content; display: inline-flex; align-items: center; min-height: 24px; padding: .18rem .45rem; border: 1px solid var(--env-rule); background: var(--env-soft); color: #67635d; font-family: var(--font-mono); font-size: .64rem; letter-spacing: .05em; text-transform: uppercase; }
  .status.confirmed, .status.ready, .status.accepted, .status.imported, .status.upcoming { border-color: #b8dccc; background: var(--env-accent-soft); color: var(--env-accent); }
  .status.pending, .status.review, .status.needs_review, .status.planning { border-color: #e7d9a4; background: var(--env-pending-soft); color: var(--env-pending); }
  .status.rejected, .status.quarantined, .status.cancelled { border-color: #e9b5a4; background: var(--env-warn-soft); color: var(--env-warn); }
  .trip-notes { margin: 1rem 0; padding: .75rem; background: var(--env-soft); color: #55524d; font-size: .84rem; line-height: 1.5; }
  .booking-timeline { list-style: none; margin: 1.25rem 0 0; padding: 0; display: grid; gap: .15rem; }
  .booking-timeline > li { display: grid; grid-template-columns: 38px minmax(0,1fr); gap: .75rem; padding: .8rem 0; border-top: 1px solid var(--env-rule-soft); }
  .kind-mark { width: 36px; height: 36px; display: grid; place-items: center; border: 1px solid var(--env-ink); font-family: var(--font-mono); font-size: .65rem; }
  .booking-main { min-width: 0; }
  .booking-line { display: flex; justify-content: space-between; gap: 1rem; }
  .booking-line time { color: #55524d; font-family: var(--font-mono); font-size: .68rem; }
  .booking-main > p { margin: .2rem 0; color: var(--env-muted); font-size: .78rem; }
  .confirmation { color: #615e58; font-family: var(--font-mono); font-size: .68rem; }
  .segments { margin-top: .65rem; background: var(--env-soft); }
  .segment { display: grid; grid-template-columns: minmax(90px,.7fr) 1fr auto; gap: .75rem; padding: .55rem .7rem; border-bottom: 1px solid var(--env-rule-soft); font-size: .72rem; }
  .segment time { color: var(--env-muted); font-family: var(--font-mono); font-size: .63rem; }
  .trip-task-summary { margin-top: 1rem; padding-top: .75rem; border-top: 1px solid var(--env-rule); color: var(--env-accent); font-size: .72rem; font-weight: 600; }
  .empty-inline { margin: .85rem 0 0; color: var(--env-muted); font-size: .82rem; line-height: 1.5; }
  .quick-column { display: grid; gap: 1rem; }
  details summary { min-height: 52px; display: flex; align-items: center; justify-content: space-between; padding: .8rem 1rem; font-weight: 600; cursor: pointer; list-style: none; }
  details summary::-webkit-details-marker { display: none; }
  details[open] summary { border-bottom: 1px solid var(--env-rule); }
  .compact-form { padding: 1rem; }
  .two-fields { display: grid; grid-template-columns: 1fr 1fr; gap: .65rem; }
  .panel-title-line { min-height: 52px; display: flex; align-items: center; justify-content: space-between; gap: 1rem; padding: 0 1rem; border-bottom: 1px solid var(--env-rule); }
  .panel-title-line h3 { margin: 0; }
  .next-panel > .empty-inline { margin: 0; padding: 1rem; }
  .attention-item { display: grid; grid-template-columns: 48px 1fr; gap: .6rem; padding: .75rem 1rem; border-bottom: 1px solid var(--env-rule-soft); font-size: .78rem; }
  .attention-item span { color: var(--env-muted); font-family: var(--font-mono); font-size: .63rem; text-transform: uppercase; }
  .alert-item span { color: var(--env-warn); }
  .empty-state { min-height: 300px; display: flex; flex-direction: column; align-items: center; justify-content: center; padding: 2rem; text-align: center; }
  .empty-state .empty-mark { width: 54px; height: 54px; display: grid; place-items: center; margin-bottom: 1rem; border: 1px solid var(--env-rule); color: var(--env-accent); font-family: var(--font-mono); font-size: 1.35rem; }
  .empty-state p { max-width: 480px; color: var(--env-muted); }
  .receipts-layout { grid-template-columns: minmax(0,1fr) 420px; }
  .receipt-list { display: grid; gap: .65rem; }
  .receipt-card { padding: 1.1rem 1.25rem; background: var(--env-surface); border: 1px solid var(--env-rule); }
  .receipt-meta { display: flex; align-items: center; gap: .55rem; margin-bottom: .65rem; color: var(--env-muted); font-family: var(--font-mono); font-size: .63rem; text-transform: uppercase; }
  .receipt-card h3 { margin-bottom: .25rem; }
  .excerpt { display: -webkit-box; overflow: hidden; margin: .9rem 0 0; color: #5b5852; font-size: .8rem; line-height: 1.5; -webkit-box-orient: vertical; -webkit-line-clamp: 3; line-clamp: 3; }
  .receipt-trip { margin: .75rem 0 0; color: var(--env-accent); font-size: .72rem; font-weight: 600; }
  .quarantine { margin: .75rem 0 0; padding: .55rem; background: var(--env-warn-soft); color: var(--env-warn); font-size: .72rem; }
  .receipt-review-form { margin-top: 1rem; padding-top: 1rem; border-top: 1px solid var(--env-rule); }
  .review-label { margin-bottom: .75rem; color: var(--env-accent); font-family: var(--font-mono); font-size: .67rem; letter-spacing: .05em; text-transform: uppercase; }
  .receipt-fields { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: .65rem; }
  .receipt-fields label { min-width: 0; margin-bottom: 0; }
  .receipt-fields .span-two { grid-column: span 2; }
  .receipt-field { min-width: 0; display: grid; align-content: start; gap: .15rem; }
  .clear-value { min-height: 44px; display: flex; flex-direction: row; align-items: center; gap: .45rem; cursor: pointer; }
  .clear-value input { width: 20px; min-height: 20px; padding: 0; accent-color: var(--env-accent); }
  .clear-value span { color: var(--env-muted); font-size: .68rem; font-weight: 500; }
  .receipt-actions { display: flex; gap: .55rem; margin-top: .85rem; }
  .paste-card { position: sticky; top: 1rem; padding: 1.4rem; }
  .paste-card > p:not(.eyebrow) { color: var(--env-muted); font-size: .83rem; line-height: 1.5; }
  .paste-card > small { display: block; margin-top: .7rem; }
  .coordinate-grid { max-width: 1440px; margin: auto; display: grid; grid-template-columns: 1fr 1fr; gap: 1.25rem; align-items: start; }
  .coordinate-panel, .sharing-panel { min-width: 0; }
  .sharing-panel { grid-column: 1 / -1; padding-bottom: 1rem; }
  .section-heading.compact { min-height: 70px; margin: 0; padding: .9rem 1rem; border-bottom: 1px solid var(--env-rule); align-items: center; }
  .section-heading.compact .eyebrow { margin-bottom: .25rem; }
  .count-badge { padding: .2rem .45rem; background: var(--env-soft); color: var(--env-muted); font-family: var(--font-mono); font-size: .65rem; text-transform: uppercase; }
  .padded { padding: 1rem; }
  .action-row { min-height: 78px; display: grid; grid-template-columns: 60px 1fr auto; align-items: center; gap: .75rem; padding: .8rem 1rem; border-bottom: 1px solid var(--env-rule-soft); }
  .action-row.done { opacity: .55; }
  .action-row p { margin: .18rem 0 0; color: var(--env-muted); font-size: .76rem; }
  .action-row time { color: var(--env-muted); font-family: var(--font-mono); font-size: .64rem; }
  .severity { justify-self: start; padding: .2rem .35rem; border: 1px solid var(--env-rule); font-family: var(--font-mono); font-size: .59rem; text-transform: uppercase; }
  .severity.warning, .severity.urgent, .severity.error, .severity.attention { border-color: #e9b5a4; color: var(--env-warn); background: var(--env-warn-soft); }
  .severity.info { color: var(--env-accent); background: var(--env-accent-soft); }
  .resolved { color: var(--env-muted); font-family: var(--font-mono); font-size: .68rem; }
  .task-row { min-height: 60px; display: flex; grid-template-columns: auto 1fr; flex-direction: row; align-items: center; gap: .8rem; margin: 0; padding: .7rem 1rem; border-bottom: 1px solid var(--env-rule-soft); cursor: pointer; }
  .task-row input, .check-label input { width: 20px; min-height: 20px; accent-color: var(--env-accent); }
  .task-row > span { display: grid; gap: .15rem; }
  .task-row small { color: var(--env-muted); font-size: .7rem; }
  .task-row.done strong { color: var(--env-muted); text-decoration: line-through; }
  .inline-task-form { display: grid; grid-template-columns: 1.4fr 1fr 1fr auto; align-items: end; gap: .6rem; padding: 1rem; }
  .inline-task-form label { margin: 0; }
  .section-copy { max-width: 780px; margin: 1rem; color: var(--env-muted); font-size: .84rem; line-height: 1.5; }
  .share-form { display: grid; grid-template-columns: 1.2fr 1fr 1fr; align-items: end; gap: .75rem; padding: 0 1rem 1rem; }
  .share-form label { margin: 0; }
  .share-form .check-label { grid-column: 1 / 3; align-self: center; }
  .check-label { min-height: 44px; display: flex; flex-direction: row; align-items: center; gap: .65rem; margin: 0; cursor: pointer; }
  .new-share { margin: 0 1rem 1rem; padding: 1rem; border: 1px solid #b8dccc; background: var(--env-accent-soft); }
  .new-share > p { margin: .2rem 0 .75rem; color: #527263; font-size: .76rem; }
  .copy-row { display: grid; grid-template-columns: 1fr auto; margin-top: .5rem; }
  .copy-row input { border-color: #a9cdbc; background: #fff; font-family: var(--font-mono); font-size: .68rem; }
  .share-list { margin: 0 1rem; border-top: 1px solid var(--env-rule); }
  .share-row { min-height: 70px; display: grid; grid-template-columns: 1fr auto; align-items: center; gap: .75rem; border-bottom: 1px solid var(--env-rule-soft); }
  .share-row p { margin: .2rem 0 0; color: var(--env-muted); font-family: var(--font-mono); font-size: .64rem; }
  .settings-grid { grid-template-columns: minmax(320px,650px) minmax(320px,1fr); }
  .settings-card, .security-card { padding: 1.4rem; }
  .security-card dl { margin: 1rem 0; border-top: 1px solid var(--env-rule); }
  .security-card dl div { display: grid; grid-template-columns: 150px 1fr; gap: 1rem; padding: .7rem 0; border-bottom: 1px solid var(--env-rule-soft); }
  .security-card dt { color: var(--env-muted); font-size: .75rem; }
  .security-card dd { margin: 0; font-family: var(--font-mono); font-size: .72rem; }

  @media (max-width: 960px) {
    .travel-header { align-items: flex-start; flex-direction: column; }
    .header-actions { justify-content: flex-start; }
    .onboarding-grid, .workspace-grid, .receipts-layout, .settings-grid { grid-template-columns: 1fr; }
    .onboarding-grid { margin-top: 5rem; }
    .paste-card { position: static; }
    .coordinate-grid { grid-template-columns: 1fr; }
    .sharing-panel { grid-column: auto; }
    .inline-task-form { grid-template-columns: 1fr 1fr; }
    .share-form { grid-template-columns: 1fr 1fr; }
    .share-form .check-label { grid-column: 1 / -1; }
  }

  @media (max-width: 640px) {
    .travel-page { padding: .85rem; }
    h1 { font-size: 2.35rem; }
    .travel-header { margin-bottom: 1rem; gap: 1rem; }
    .header-actions, .header-actions > * { width: 100%; }
    .connection-pill { justify-content: center; }
    .view-tabs button { min-height: 52px; padding: 0 .8rem; }
    .summary-strip { grid-template-columns: 1fr 1fr; }
    .summary-strip div { min-height: 75px; border-bottom: 1px solid #393939; }
    .trip-body { padding: 1rem; }
    .trip-card-head, .booking-line { flex-direction: column; }
    .segment { grid-template-columns: 1fr; }
    .section-heading { align-items: flex-start; flex-direction: column; }
    .two-fields, .inline-task-form, .share-form { grid-template-columns: 1fr; }
    .share-form .check-label { grid-column: auto; }
    .action-row { grid-template-columns: 52px 1fr; }
    .action-row .button, .action-row .resolved { grid-column: 2; justify-self: start; }
    .copy-row { grid-template-columns: 1fr; }
    .copy-row .button { width: 100%; }
    .security-card dl div { grid-template-columns: 1fr; gap: .2rem; }
    .receipt-fields { grid-template-columns: 1fr; }
    .receipt-fields .span-two { grid-column: auto; }
    .receipt-actions { flex-direction: column; }
    .receipt-actions .button { width: 100%; }
  }
</style>
