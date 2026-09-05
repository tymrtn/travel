import { afterEach, describe, expect, it, vi } from 'vitest';
import { resetCsrf } from './api';
import {
  buildReceiptApprovalInput,
  GMAIL_IMAP_HOST,
  GMAIL_IMAP_PORT,
  GMAIL_SMTP_HOST,
  GMAIL_SMTP_PORT,
  isTravelAlertDue,
  publicTravelApi,
  sanitizePublicTravelOverview,
  summarizeReceiptImport,
  summarizeTravelSync,
  type PublicTravelOverview,
  travelApi
} from './travel-api';

type FetchCall = [RequestInfo | URL, RequestInit?];

function fetchCalls(fetchImpl: { mock: { calls: unknown[][] } }): FetchCall[] {
  return fetchImpl.mock.calls as unknown as FetchCall[];
}

function jsonResponse(body: unknown, status = 200): Response {
  const payload = JSON.stringify(body);
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => JSON.parse(payload),
    clone() {
      return jsonResponse(body, status);
    }
  } as unknown as Response;
}

function methodOf(call: FetchCall): string {
  return (call[1]?.method ?? 'GET').toUpperCase();
}

function apiCalls(fetchImpl: { mock: { calls: unknown[][] } }): FetchCall[] {
  return fetchCalls(fetchImpl).filter(([url]) => String(url) !== '/api/csrf');
}

function successfulFetch() {
  return vi.fn(async (url: RequestInfo | URL) => {
    if (String(url) === '/api/csrf') return jsonResponse({ token: 'csrf-travel' });
    return jsonResponse({});
  });
}

afterEach(() => {
  resetCsrf();
  vi.restoreAllMocks();
});

describe('travelApi route contract', () => {
  it('uses the complete authenticated travel surface and preserves request bodies', async () => {
    const fetchImpl = successfulFetch();

    await travelApi.overview({ fetchImpl });
    await travelApi.saveSettings(
      {
        account_id: 'acct-1',
        home_timezone: 'Europe/Paris',
        scan_folder: '[Gmail]/All Mail'
      },
      { fetchImpl }
    );
    await travelApi.sync(
      { account_id: 'acct-1', folder: 'Travel Receipts', limit: 200 },
      { fetchImpl }
    );
    await travelApi.importReceipt(
      {
        from_addr: 'airline@example.test',
        subject: 'Your itinerary',
        body_text: 'Confirmation ABC123'
      },
      { fetchImpl }
    );
    await travelApi.approveReceipt(
      'receipt-1',
      {
        kind: 'flight',
        title: 'Paris to New York',
        provider: 'Air France',
        confirmation_code: 'ABC123',
        status: 'changed',
        start_at: '2026-09-01T10:00:00Z',
        end_at: '2026-09-01T18:00:00Z',
        origin: 'CDG',
        destination: 'JFK'
      },
      { fetchImpl }
    );
    await travelApi.dismissReceipt('receipt-2', { fetchImpl });
    await travelApi.createTrip(
      { title: 'Paris', destination: 'Paris', timezone: 'Europe/Paris' },
      { fetchImpl }
    );
    await travelApi.createBooking(
      { trip_id: 'trip-1', kind: 'flight', title: 'Flight to Paris' },
      { fetchImpl }
    );
    await travelApi.createTask('trip-1', { title: 'Check in' }, { fetchImpl });
    await travelApi.toggleTask('task-1', { fetchImpl });
    await travelApi.acknowledgeAlert('alert-1', { fetchImpl });
    await travelApi.createShare(
      { trip_id: 'trip-1', label: 'Family', can_edit_tasks: true },
      { fetchImpl }
    );
    await travelApi.deleteShare('share-1', { fetchImpl });

    const calls = apiCalls(fetchImpl);
    expect(calls.map(([url, init]) => [String(url), (init?.method ?? 'GET').toUpperCase()])).toEqual([
      ['/api/travel/overview', 'GET'],
      ['/api/travel/settings', 'PUT'],
      ['/api/travel/sync', 'POST'],
      ['/api/travel/import', 'POST'],
      ['/api/travel/receipts/receipt-1/approve', 'POST'],
      ['/api/travel/receipts/receipt-2/dismiss', 'POST'],
      ['/api/travel/trips', 'POST'],
      ['/api/travel/bookings', 'POST'],
      ['/api/travel/trips/trip-1/tasks', 'POST'],
      ['/api/travel/tasks/task-1/toggle', 'POST'],
      ['/api/travel/alerts/alert-1/acknowledge', 'POST'],
      ['/api/travel/shares', 'POST'],
      ['/api/travel/shares/share-1', 'DELETE']
    ]);

    expect(JSON.parse(String(calls[1]![1]?.body))).toEqual({
      account_id: 'acct-1',
      home_timezone: 'Europe/Paris',
      scan_folder: '[Gmail]/All Mail'
    });
    expect(JSON.parse(String(calls[2]![1]?.body))).toEqual({
      account_id: 'acct-1',
      folder: 'Travel Receipts',
      limit: 200
    });
    expect(JSON.parse(String(calls[4]![1]?.body))).toEqual({
      kind: 'flight',
      title: 'Paris to New York',
      provider: 'Air France',
      confirmation_code: 'ABC123',
      status: 'changed',
      start_at: '2026-09-01T10:00:00Z',
      end_at: '2026-09-01T18:00:00Z',
      origin: 'CDG',
      destination: 'JFK'
    });
  });

  it('preserves explicit receipt clears as JSON null values', async () => {
    const fetchImpl = successfulFetch();
    await travelApi.approveReceipt(
      'receipt-clear',
      {
        provider: null,
        confirmation_code: null,
        end_at: null,
        origin: null,
        destination: null
      },
      { fetchImpl }
    );

    const calls = apiCalls(fetchImpl);
    expect(JSON.parse(String(calls[0]![1]?.body))).toEqual({
      provider: null,
      confirmation_code: null,
      end_at: null,
      origin: null,
      destination: null
    });
  });

  it('URL-encodes every owner and public path parameter', async () => {
    const fetchImpl = successfulFetch();

    await travelApi.createTask('trip /?#', { title: 'Pack' }, { fetchImpl });
    await travelApi.toggleTask('task /?#', { fetchImpl });
    await travelApi.acknowledgeAlert('alert /?#', { fetchImpl });
    await travelApi.deleteShare('share /?#', { fetchImpl });
    await publicTravelApi.overview('family /?#', { fetchImpl });
    await publicTravelApi.toggleTask('family /?#', 'task /?#', { fetchImpl });

    expect(apiCalls(fetchImpl).map(([url]) => String(url))).toEqual([
      '/api/travel/trips/trip%20%2F%3F%23/tasks',
      '/api/travel/tasks/task%20%2F%3F%23/toggle',
      '/api/travel/alerts/alert%20%2F%3F%23/acknowledge',
      '/api/travel/shares/share%20%2F%3F%23',
      '/api/public/travel/family%20%2F%3F%23',
      '/api/public/travel/family%20%2F%3F%23/tasks/task%20%2F%3F%23/toggle'
    ]);
  });

  it('edits a shared task without touching the protected CSRF endpoint', async () => {
    resetCsrf();
    const fetchImpl = vi.fn(async () => jsonResponse({ task: { id: 'task-1' } }));

    await publicTravelApi.toggleTask('family-token', 'task-1', { fetchImpl });

    expect(fetchCalls(fetchImpl)).toHaveLength(1);
    const [url, init] = fetchCalls(fetchImpl)[0]!;
    expect(String(url)).toBe('/api/public/travel/family-token/tasks/task-1/toggle');
    expect(init?.method).toBe('POST');
    expect(init?.credentials).toBe('omit');
    expect(init?.cache).toBe('no-store');
    expect(init?.headers).toMatchObject({ 'Content-Type': 'application/json' });
    expect(init?.body).toBe('{}');
  });

  it('never reuses a cached family itinerary after revocation or replacement', async () => {
    const fetchImpl = successfulFetch();

    await publicTravelApi.overview('family-token', { fetchImpl });

    const [url, init] = fetchCalls(fetchImpl)[0]!;
    expect(String(url)).toBe('/api/public/travel/family-token');
    expect(init?.method).toBe('GET');
    expect(init?.cache).toBe('no-store');
  });
});

describe('travel action feedback', () => {
  it('reports successful sync counts without inventing a generic success', () => {
    const summary = summarizeTravelSync({
      status: 'complete',
      folders_scanned: ['INBOX'],
      messages_examined: 18,
      receipts_imported: 3,
      bookings_created: 2,
      needs_review: 1,
      errors: []
    });

    expect(summary.tone).toBe('success');
    expect(summary.message).toContain('Mailbox check complete');
    expect(summary.message).toContain('18 messages examined');
    expect(summary.message).toContain('3 receipts imported');
    expect(summary.message).toContain('2 bookings created');
    expect(summary.message).toContain('1 item needs review');
    expect(summary.message).toContain('Folders: INBOX');
  });

  it('surfaces partial sync status, useful counts, and backend issue messages', () => {
    const summary = summarizeTravelSync({
      status: 'partial',
      messages_examined: 40,
      receipts_imported: 4,
      bookings_created: 1,
      needs_review: 2,
      errors: ['[Gmail]/All Mail: connection reset', { message: 'INBOX scan timed out' }]
    });

    expect(summary.tone).toBe('warning');
    expect(summary.message).toContain('Mailbox check partial');
    expect(summary.message).toContain('40 messages examined');
    expect(summary.message).toContain('4 receipts imported');
    expect(summary.message).toContain('[Gmail]/All Mail: connection reset');
    expect(summary.message).toContain('INBOX scan timed out');
  });

  it('distinguishes failed, review, and deduplicated receipt imports', () => {
    expect(
      summarizeReceiptImport({ status: 'failed', message: 'Parser unavailable' })
    ).toEqual({ tone: 'warning', message: 'Receipt import failed: Parser unavailable' });

    expect(summarizeReceiptImport({ inserted: true, needs_review: true })).toEqual({
      tone: 'success',
      message: 'Receipt imported for review.'
    });

    const duplicate = summarizeReceiptImport({ inserted: false, deduped_by: 'content_hash' });
    expect(duplicate.tone).toBe('success');
    expect(duplicate.message).toContain('already on file');
    expect(duplicate.message).toContain('content hash');
  });
});

describe('travel review decisions', () => {
  it('builds a corrected receipt approval payload from every editable field', () => {
    const input = buildReceiptApprovalInput({
      kind: 'flight',
      title: '  Paris to New York  ',
      provider: ' Air France ',
      confirmation_code: ' ABC123 ',
      status: 'changed',
      start_at: '2026-09-01T10:00:00Z',
      end_at: '2026-09-01T18:00:00Z',
      origin: ' CDG ',
      destination: ' JFK ',
      clear_provider: false,
      clear_confirmation_code: false,
      clear_end_at: false,
      clear_origin: false,
      clear_destination: false
    });

    expect(input).toEqual({
      kind: 'flight',
      title: 'Paris to New York',
      provider: 'Air France',
      confirmation_code: 'ABC123',
      status: 'changed',
      start_at: '2026-09-01T10:00:00.000Z',
      end_at: '2026-09-01T18:00:00.000Z',
      origin: 'CDG',
      destination: 'JFK'
    });
  });

  it('keeps untouched blank overrides omitted, including the masked confirmation', () => {
    const input = buildReceiptApprovalInput({
      kind: 'hotel',
      title: 'Hotel',
      provider: '',
      confirmation_code: '',
      status: 'confirmed',
      start_at: '',
      end_at: '',
      origin: '',
      destination: '',
      clear_provider: false,
      clear_confirmation_code: false,
      clear_end_at: false,
      clear_origin: false,
      clear_destination: false
    });

    expect(input).toEqual({
      kind: 'hotel',
      title: 'Hotel',
      status: 'confirmed',
      start_at: undefined
    });
  });

  it('sends explicit nulls for optional extracted values selected for clearing', () => {
    const input = buildReceiptApprovalInput({
      kind: 'flight',
      title: 'Flight',
      provider: 'Ignored provider',
      confirmation_code: 'IGNORED42',
      status: 'confirmed',
      start_at: '2026-09-01T10:00:00Z',
      end_at: '2026-09-01T18:00:00Z',
      origin: 'CDG',
      destination: 'JFK',
      clear_provider: true,
      clear_confirmation_code: true,
      clear_end_at: true,
      clear_origin: true,
      clear_destination: true
    });

    expect(input).toEqual({
      kind: 'flight',
      title: 'Flight',
      provider: null,
      confirmation_code: null,
      status: 'confirmed',
      start_at: '2026-09-01T10:00:00.000Z',
      end_at: null,
      origin: null,
      destination: null
    });
  });

  it('shows only unacknowledged alerts that have triggered or are due', () => {
    const now = Date.parse('2026-09-01T12:00:00Z');
    expect(isTravelAlertDue({ scheduled_at: '2026-09-01T11:59:00Z' }, now)).toBe(true);
    expect(isTravelAlertDue({ scheduled_at: '2026-09-01T12:01:00Z' }, now)).toBe(false);
    expect(
      isTravelAlertDue(
        { triggered_at: '2026-09-01T10:00:00Z', scheduled_at: '2026-09-02T10:00:00Z' },
        now
      )
    ).toBe(true);
    expect(
      isTravelAlertDue(
        { triggered_at: '2026-09-01T10:00:00Z', acknowledged_at: '2026-09-01T11:00:00Z' },
        now
      )
    ).toBe(false);
  });
});

describe('public travel projection', () => {
  it('retains only family-safe fields even if a response grows private data', () => {
    const safe = sanitizePublicTravelOverview({
      can_edit_tasks: true,
      calendar_url: '/api/public/travel/calendar_read_only/calendar.ics',
      trip: {
        id: 'trip-1',
        title: 'Paris',
        destination: 'Paris',
        timezone: 'Europe/Paris',
        notes: 'private trip note'
      },
      bookings: [
        {
          id: 'booking-1',
          trip_id: 'trip-1',
          kind: 'hotel',
          title: 'Hotel',
          confirmation_code: 'DO-NOT-SHARE',
          confirmation_masked: '•••• HARE',
          details: { guest_email: 'private@example.test' }
        }
      ],
      segments: [],
      tasks: [
        {
          id: 'task-1',
          title: 'Pack',
          notes: 'private task note'
        }
      ],
      alerts: [
        {
          id: 'alert-1',
          kind: 'change',
          title: 'Schedule changed',
          body: 'private alert body'
        }
      ]
    } as PublicTravelOverview);

    expect(safe.canEditTasks).toBe(true);
    expect(safe.calendarUrl).toBe('/api/public/travel/calendar_read_only/calendar.ics');
    expect(safe.trip).toEqual({
      id: 'trip-1',
      title: 'Paris',
      destination: 'Paris',
      starts_at: undefined,
      ends_at: undefined,
      timezone: 'Europe/Paris',
      status: undefined
    });
    expect(safe.tasks[0]).toEqual({
      id: 'task-1',
      title: 'Pack',
      due_at: undefined,
      completed_at: undefined,
      completed: false
    });
    expect(safe.alerts[0]).toEqual({
      id: 'alert-1',
      severity: undefined,
      title: 'Schedule changed',
      scheduled_at: undefined,
      triggered_at: undefined
    });
    const serialized = JSON.stringify(safe);
    for (const privateValue of [
      'DO-NOT-SHARE',
      'HARE',
      'private@example.test',
      'private trip note',
      'private task note',
      'private alert body'
    ]) {
      expect(serialized).not.toContain(privateValue);
    }
  });
});

describe('Gmail onboarding secret boundary', () => {
  it('sends the app password only to the atomic Gmail connection endpoint', async () => {
    const appPassword = 'abcd efgh ijkl mnop';
    const storageWrite = vi.spyOn(Storage.prototype, 'setItem');
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      const path = String(url);
      if (path === '/api/csrf') return jsonResponse({ token: 'csrf-gmail' });
      if (path === '/api/travel/connect-gmail') {
        return jsonResponse({
          account: {
            id: 'gmail /?#',
            name: 'Family Travel',
            username: 'traveler@gmail.com',
            domain: 'gmail.com',
            smtp_host: GMAIL_SMTP_HOST,
            smtp_port: GMAIL_SMTP_PORT,
            imap_host: GMAIL_IMAP_HOST,
            imap_port: GMAIL_IMAP_PORT
          },
          verification: { ok: true, imap: true, smtp: false, error: null },
          settings: {
            account_id: 'gmail /?#',
            timezone: 'Europe/Paris',
            calendar_name: 'Martin travel'
          }
        });
      }
      throw new Error(`unexpected request: ${path}`);
    });

    const result = await travelApi.connectGmail(
      {
        email: 'traveler@gmail.com',
        password: appPassword,
        display_name: 'Family Travel',
        settings: {
          timezone: 'Europe/Paris',
          calendar_name: 'Martin travel',
          scan_folder: '[Gmail]/All Mail'
        }
      },
      { fetchImpl }
    );

    const calls = apiCalls(fetchImpl);
    expect(calls.map(([url]) => String(url))).toEqual(['/api/travel/connect-gmail']);

    const accountBody = JSON.parse(String(calls[0]![1]?.body));
    expect(accountBody).toEqual({
      email: 'traveler@gmail.com',
      password: appPassword,
      display_name: 'Family Travel',
      settings: {
        timezone: 'Europe/Paris',
        calendar_name: 'Martin travel',
        scan_folder: '[Gmail]/All Mail'
      }
    });
    expect(String(calls[0]![0])).not.toContain(appPassword);
    expect(JSON.stringify(result)).not.toContain(appPassword);
    expect(storageWrite).not.toHaveBeenCalled();
    expect(result.settings?.account_id).toBe('gmail /?#');
  });

  it('does not select an account whose Gmail verification failed', async () => {
    const fetchImpl = vi.fn(async (url: RequestInfo | URL) => {
      const path = String(url);
      if (path === '/api/csrf') return jsonResponse({ token: 'csrf-gmail' });
      if (path === '/api/travel/connect-gmail') {
        return jsonResponse({
          account: {
            id: 'bad-gmail',
            name: 'Gmail',
            username: 'traveler@gmail.com',
            domain: 'gmail.com',
            smtp_host: GMAIL_SMTP_HOST,
            smtp_port: GMAIL_SMTP_PORT,
            imap_host: GMAIL_IMAP_HOST,
            imap_port: GMAIL_IMAP_PORT
          },
          verification: {
            ok: false,
            imap: false,
            smtp: false,
            error: 'authentication failed'
          },
          settings: null
        });
      }
      throw new Error(`unexpected request: ${path}`);
    });

    const result = await travelApi.connectGmail(
      { email: 'traveler@gmail.com', password: 'wrong app password' },
      { fetchImpl }
    );

    expect(result.settings).toBeNull();
    expect(apiCalls(fetchImpl).map(([url]) => String(url))).toEqual([
      '/api/travel/connect-gmail'
    ]);
    expect(methodOf(apiCalls(fetchImpl)[0]!)).toBe('POST');
  });
});
