<script lang="ts">
  import { onMount } from 'svelte';
  import { page } from '$app/state';
  import {
    isTravelAlertDue,
    publicTravelApi,
    sanitizePublicTravelOverview,
    type SafePublicBooking,
    type SafePublicTask,
    type SafePublicTravelOverview,
    type SafePublicTrip,
    type SafePublicSegment
  } from '$lib/travel-api';

  let shared = $state<SafePublicTravelOverview | null>(null);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let busyTask = $state<string | null>(null);
  let copied = $state(false);
  let alertClock = $state(Date.now());

  const token = $derived(page.params.token ?? '');
  const calendarUrl = $derived(shared?.calendarUrl ?? '');
  const visibleAlerts = $derived(
    shared?.alerts.filter((alert) => isTravelAlertDue(alert, alertClock)) ?? []
  );

  onMount(() => {
    void load();
    const timer = window.setInterval(() => (alertClock = Date.now()), 30_000);
    return () => window.clearInterval(timer);
  });

  async function load() {
    loading = true;
    error = null;
    try {
      const response = await publicTravelApi.overview(token);
      // Deliberate allowlist: shared pages never retain or render confirmation
      // codes, raw receipt data, parser metadata, or owner-only notes.
      shared = sanitizePublicTravelOverview(response);
    } catch (cause) {
      error = cause instanceof Error ? cause.message : 'This private trip link is unavailable.';
    } finally {
      loading = false;
    }
  }

  async function toggleTask(task: SafePublicTask) {
    if (!shared?.canEditTasks) return;
    busyTask = task.id;
    error = null;
    try {
      await publicTravelApi.toggleTask(token, task.id);
      await load();
    } catch (cause) {
      error = cause instanceof Error ? cause.message : 'The task could not be updated.';
    } finally {
      busyTask = null;
    }
  }

  async function copyPageLink() {
    try {
      await navigator.clipboard.writeText(window.location.href);
      copied = true;
      window.setTimeout(() => (copied = false), 1800);
    } catch {
      error = 'Copy was blocked by the browser. Copy the address from the address bar instead.';
    }
  }

  function titleCase(value: string): string {
    return (value || 'travel').replaceAll('_', ' ').replace(/\b\w/g, (letter) => letter.toUpperCase());
  }

  function formatDate(value?: string, includeTime = false): string {
    if (!value) return 'Time to be confirmed';
    const date = new Date(value);
    if (Number.isNaN(date.getTime())) return value;
    return new Intl.DateTimeFormat(undefined, {
      month: 'short', day: 'numeric', year: includeTime ? undefined : 'numeric',
      hour: includeTime ? 'numeric' : undefined, minute: includeTime ? '2-digit' : undefined
    }).format(date);
  }

  function dateRange(trip: SafePublicTrip): string {
    if (!trip.starts_at && !trip.ends_at) return 'Dates to be confirmed';
    if (!trip.ends_at) return formatDate(trip.starts_at);
    return `${formatDate(trip.starts_at)} — ${formatDate(trip.ends_at)}`;
  }

  function segmentsFor(booking: SafePublicBooking): SafePublicSegment[] {
    return shared?.segments.filter((segment) => segment.booking_id === booking.id) ?? [];
  }
</script>

<svelte:head>
  <title>{shared?.trip.title || 'Shared trip'} · Travel Travel</title>
  <meta name="robots" content="noindex,nofollow,noarchive" />
  <meta name="referrer" content="no-referrer" />
</svelte:head>

<div class="share-page">
  {#if loading}
    <div class="loading" aria-live="polite"><span aria-hidden="true"></span>Opening the shared trip…</div>
  {:else if error && !shared}
    <section class="unavailable">
      <p class="eyebrow">Travel travel</p>
      <h1>This private link isn’t available.</h1>
      <p>It may have expired or been revoked. Ask the trip organizer for a fresh link.</p>
      <button onclick={load}>Try again</button>
    </section>
  {:else if shared}
    <header class="hero">
      <div class="hero-inner">
        <div>
          <p class="eyebrow">Shared travel plan</p>
          <h1>{shared.trip.title}</h1>
          <p class="destination">{shared.trip.destination || 'Destination to be confirmed'}</p>
        </div>
        <div class="date-block"><span>Travel dates</span><strong>{dateRange(shared.trip)}</strong></div>
      </div>
    </header>

    {#if error}<div class="flash" role="alert">{error}<button aria-label="Dismiss error" onclick={() => (error = null)}>×</button></div>{/if}

    <main class="shared-workspace">
      <section class="itinerary">
        <div class="section-head">
          <div><p class="eyebrow">Live itinerary</p><h2>The plan</h2></div>
          <span>{shared.bookings.length} {shared.bookings.length === 1 ? 'booking' : 'bookings'}</span>
        </div>

        {#if visibleAlerts.length > 0}
          <div class="alerts" aria-label="Trip alerts">
            {#each visibleAlerts as alert (alert.id)}
              <article><span>{alert.severity || 'update'}</span><div><strong>{alert.title}</strong></div></article>
            {/each}
          </div>
        {/if}

        {#if shared.bookings.length === 0}
          <div class="empty"><h3>The itinerary is being assembled.</h3><p>Bookings will appear here when the organizer adds them.</p></div>
        {:else}
          <ol class="timeline">
            {#each shared.bookings as booking, index (booking.id)}
              <li>
                <div class="marker"><span>{String(index + 1).padStart(2, '0')}</span></div>
                <article class="booking">
                  <header>
                    <div><p>{titleCase(booking.kind)}</p><h3>{booking.title}</h3></div>
                    <time>{formatDate(booking.starts_at, true)}</time>
                  </header>
                  <div class="route">
                    <strong>{booking.origin || booking.location || booking.provider || 'Details to follow'}</strong>
                    {#if booking.destination}<span aria-hidden="true">→</span><strong>{booking.destination}</strong>{/if}
                  </div>
                  <div class="metadata">
                    {#if booking.provider}<span>{booking.provider}</span>{/if}
                    {#if booking.service_number}<span>{booking.service_number}</span>{/if}
                    {#if booking.ends_at}<span>Until {formatDate(booking.ends_at, true)}</span>{/if}
                  </div>
                  {#if segmentsFor(booking).length > 0}
                    <div class="segments">
                      {#each segmentsFor(booking) as segment (segment.id)}
                        <div>
                          <strong>{segment.title || titleCase(segment.kind)}</strong>
                          <span>{segment.origin || segment.location || '—'} → {segment.destination || '—'}</span>
                          <time>{formatDate(segment.departs_at, true)}{segment.arrives_at ? ` — ${formatDate(segment.arrives_at, true)}` : ''}</time>
                        </div>
                      {/each}
                    </div>
                  {/if}
                </article>
              </li>
            {/each}
          </ol>
        {/if}
      </section>

      <aside>
        <section class="side-card calendar-card">
          <p class="eyebrow">Keep it close</p>
          <h2>Put this trip on your calendar.</h2>
          <p>Subscribe once to receive itinerary changes from the organizer.</p>
          {#if calendarUrl}<a class="primary-action" href={calendarUrl}>Subscribe to calendar</a>{/if}
          <button class="secondary-action" onclick={copyPageLink}>{copied ? 'Link copied' : 'Copy trip link'}</button>
        </section>

        <section class="side-card tasks-card">
          <div class="side-heading"><div><p class="eyebrow">Family checklist</p><h2>To do</h2></div><span>{shared.tasks.filter((task) => !task.completed).length}</span></div>
          {#if shared.tasks.length === 0}
            <p class="task-empty">Nothing on the checklist yet.</p>
          {:else}
            <div class="tasks">
              {#each shared.tasks as task (task.id)}
                <label class:complete={task.completed}>
                  <input
                    type="checkbox"
                    checked={task.completed}
                    disabled={!shared.canEditTasks || busyTask === task.id}
                    onchange={() => toggleTask(task)}
                  />
                  <span><strong>{task.title}</strong>{#if task.due_at}<small>Due {formatDate(task.due_at, true)}</small>{/if}</span>
                </label>
              {/each}
            </div>
          {/if}
          <p class="permission-note">{shared.canEditTasks ? 'You can check off shared tasks.' : 'Only the organizer can update this checklist.'}</p>
        </section>
      </aside>
    </main>

    <footer>
      <span>Shared privately with Travel</span>
      <span>Booking confirmation numbers are hidden</span>
    </footer>
  {/if}
</div>

<style>
  .share-page { width: 100%; min-height: 100%; overflow: auto; background: #f3f1ec; }
  .eyebrow { margin: 0 0 .5rem; color: var(--env-accent); font-family: var(--font-mono); font-size: .66rem; letter-spacing: .17em; text-transform: uppercase; }
  h1, h2, h3, p { margin-top: 0; }
  .hero { padding: clamp(2.25rem, 7vw, 6rem) clamp(1rem, 5vw, 4rem) 2rem; background: var(--env-ink); color: white; }
  .hero-inner { max-width: 1220px; margin: auto; display: flex; align-items: flex-end; justify-content: space-between; gap: 3rem; }
  .hero h1 { margin-bottom: .4rem; font-size: clamp(2.5rem, 7vw, 5.5rem); line-height: .94; letter-spacing: -.055em; font-weight: 500; }
  .destination { margin: 0; color: #bcb8b0; font-size: 1.1rem; }
  .date-block { min-width: 250px; display: grid; gap: .35rem; padding: 1rem 0; border-top: 1px solid #555; border-bottom: 1px solid #555; }
  .date-block span { color: #9d9992; font-family: var(--font-mono); font-size: .64rem; letter-spacing: .1em; text-transform: uppercase; }
  .date-block strong { font-weight: 500; }
  .flash { max-width: 1220px; min-height: 48px; margin: 1rem auto 0; padding: 0 1rem; display: flex; align-items: center; justify-content: space-between; background: var(--env-warn-soft); border: 1px solid #e9b5a4; color: var(--env-warn); font-size: .8rem; }
  .flash button { width: 44px; height: 44px; border: 0; background: transparent; color: inherit; font-size: 1.2rem; cursor: pointer; }
  .shared-workspace { max-width: 1220px; margin: 0 auto; padding: 2rem 1rem 4rem; display: grid; grid-template-columns: minmax(0,1fr) 340px; gap: 2rem; align-items: start; }
  .section-head { min-height: 60px; display: flex; align-items: flex-end; justify-content: space-between; border-bottom: 1px solid var(--env-ink); }
  .section-head h2 { margin-bottom: .65rem; font-size: 1.8rem; }
  .section-head > span { margin-bottom: .85rem; color: var(--env-muted); font-family: var(--font-mono); font-size: .65rem; text-transform: uppercase; }
  .alerts { margin: 1rem 0; border: 1px solid #e9b5a4; }
  .alerts article { display: grid; grid-template-columns: 70px 1fr; gap: 1rem; padding: .8rem; background: var(--env-warn-soft); border-bottom: 1px solid #efcabb; }
  .alerts article:last-child { border: 0; }
  .alerts article > span { color: var(--env-warn); font-family: var(--font-mono); font-size: .6rem; text-transform: uppercase; }
  .timeline { list-style: none; margin: 0; padding: 0; }
  .timeline > li { display: grid; grid-template-columns: 55px minmax(0,1fr); }
  .marker { display: flex; justify-content: center; position: relative; }
  .marker::before { content: ''; position: absolute; inset: 0 auto; width: 1px; background: var(--env-rule); }
  .marker span { position: relative; width: 30px; height: 30px; margin-top: 1.4rem; display: grid; place-items: center; background: #f3f1ec; border: 1px solid var(--env-accent); color: var(--env-accent); font-family: var(--font-mono); font-size: .6rem; }
  .booking { margin: .75rem 0; padding: 1.25rem; background: white; border: 1px solid var(--env-rule); }
  .booking header { display: flex; justify-content: space-between; gap: 1rem; }
  .booking header p { margin-bottom: .2rem; color: var(--env-accent); font-family: var(--font-mono); font-size: .62rem; letter-spacing: .1em; text-transform: uppercase; }
  .booking header h3 { margin-bottom: 0; font-size: 1.15rem; }
  .booking header time { color: #4f4c47; font-family: var(--font-mono); font-size: .66rem; }
  .route { display: flex; align-items: center; gap: .65rem; margin-top: 1.25rem; font-size: .9rem; }
  .route span { color: var(--env-accent); }
  .metadata { display: flex; flex-wrap: wrap; gap: .4rem 1rem; margin-top: .6rem; color: var(--env-muted); font-family: var(--font-mono); font-size: .62rem; }
  .segments { margin-top: 1rem; border-top: 1px solid var(--env-rule-soft); }
  .segments > div { display: grid; grid-template-columns: .8fr 1fr auto; gap: .75rem; padding: .7rem 0; border-bottom: 1px solid var(--env-rule-soft); font-size: .7rem; }
  .segments time { color: var(--env-muted); font-family: var(--font-mono); font-size: .6rem; }
  aside { display: grid; gap: 1rem; position: sticky; top: 1rem; }
  .side-card { padding: 1.25rem; background: white; border: 1px solid var(--env-rule); }
  .side-card h2 { font-size: 1.35rem; line-height: 1.1; }
  .calendar-card > p:not(.eyebrow) { color: var(--env-muted); font-size: .8rem; line-height: 1.5; }
  .primary-action, .secondary-action, .unavailable button { width: 100%; min-height: 46px; display: flex; align-items: center; justify-content: center; border: 1px solid var(--env-ink); border-radius: 2px; padding: .65rem .9rem; font-size: .8rem; font-weight: 600; text-decoration: none; cursor: pointer; }
  .primary-action { background: var(--env-ink); color: white; }
  .secondary-action { margin-top: .55rem; background: white; color: var(--env-ink); }
  .side-heading { display: flex; align-items: flex-end; justify-content: space-between; gap: 1rem; border-bottom: 1px solid var(--env-rule); }
  .side-heading h2 { margin-bottom: .75rem; }
  .side-heading > span { width: 26px; height: 26px; margin-bottom: .75rem; display: grid; place-items: center; background: var(--env-soft); font-family: var(--font-mono); font-size: .65rem; }
  .tasks label { min-height: 61px; display: flex; align-items: center; gap: .75rem; border-bottom: 1px solid var(--env-rule-soft); cursor: pointer; }
  .tasks input { width: 20px; height: 20px; accent-color: var(--env-accent); }
  .tasks label > span { display: grid; gap: .15rem; }
  .tasks small { color: var(--env-muted); font-size: .65rem; }
  .tasks label.complete strong { color: var(--env-muted); text-decoration: line-through; }
  .task-empty, .permission-note { margin: .9rem 0 0; color: var(--env-muted); font-size: .72rem; line-height: 1.45; }
  .permission-note { padding-top: .8rem; border-top: 1px solid var(--env-rule); }
  .empty { margin-top: 1rem; padding: 3rem 1rem; background: white; border: 1px solid var(--env-rule); text-align: center; }
  .empty p { color: var(--env-muted); }
  footer { min-height: 70px; display: flex; align-items: center; justify-content: center; gap: 1rem 3rem; padding: 1rem; background: #e9e6df; color: #77736d; font-family: var(--font-mono); font-size: .6rem; letter-spacing: .06em; text-transform: uppercase; }
  .loading { min-height: 65vh; display: flex; align-items: center; justify-content: center; gap: .7rem; color: var(--env-muted); }
  .loading span { width: 16px; height: 16px; border: 2px solid var(--env-rule); border-top-color: var(--env-accent); border-radius: 50%; animation: spin .7s linear infinite; }
  @keyframes spin { to { transform: rotate(360deg); } }
  .unavailable { max-width: 640px; margin: 10vh auto; padding: 3rem; background: white; border: 1px solid var(--env-rule); text-align: center; }
  .unavailable h1 { font-size: 2.2rem; letter-spacing: -.04em; }
  .unavailable > p:not(.eyebrow) { color: var(--env-muted); line-height: 1.55; }
  .unavailable button { max-width: 180px; margin: 1.5rem auto 0; background: var(--env-ink); color: white; }

  @media (max-width: 820px) {
    .hero-inner { align-items: flex-start; flex-direction: column; }
    .date-block { width: 100%; }
    .shared-workspace { grid-template-columns: 1fr; }
    aside { position: static; grid-row: 1; grid-template-columns: 1fr 1fr; }
  }
  @media (max-width: 600px) {
    .hero { padding: 2.5rem 1rem 1.5rem; }
    .hero h1 { font-size: 2.8rem; }
    .shared-workspace { padding: 1rem .75rem 2.5rem; }
    aside { grid-template-columns: 1fr; }
    .timeline > li { grid-template-columns: 38px minmax(0,1fr); }
    .marker span { width: 25px; height: 25px; }
    .booking { padding: 1rem; }
    .booking header { flex-direction: column; }
    .segments > div { grid-template-columns: 1fr; }
    footer { flex-direction: column; gap: .5rem; }
    .unavailable { margin: 2rem .75rem; padding: 2rem 1rem; }
  }
</style>
