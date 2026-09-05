// Typed API client for the local-first travel workspace.
//
// Travel extraction intentionally evolves independently of the dashboard UI,
// so the entities below describe the stable fields and leave room for parser-
// specific metadata. Mutations all go through the shared request() client for
// the dashboard's CSRF and structured-error handling.

import {
  EnvelopeApiError,
  request,
  type Account,
  type RequestOptions,
  type VerifyResult
} from './api';

export type TravelMetadata = Record<string, unknown>;

export interface TravelSettings {
  id?: number;
  account_id: string | null;
  scan_folder?: string;
  home_timezone?: string;
  /** Accepted by early travel API builds as an alias for home_timezone. */
  timezone?: string;
  calendar_name?: string;
  auto_ingest?: boolean;
  default_alert_minutes?: number;
  updated_at?: string;
  account_email?: string;
  last_synced_at?: string;
  [key: string]: unknown;
}

export interface Trip {
  id: string;
  title: string;
  destination?: string | null;
  starts_at?: string | null;
  ends_at?: string | null;
  /** Aliases accepted by early parser payloads. */
  start_at?: string | null;
  end_at?: string | null;
  timezone?: string;
  status?: string;
  notes?: string | null;
  created_at?: string;
  updated_at?: string;
  bookings?: Booking[];
  tasks?: TravelTask[];
  [key: string]: unknown;
}

export interface Booking {
  id: string;
  trip_id: string;
  receipt_id?: string | null;
  kind: string;
  provider?: string | null;
  title: string;
  confirmation_code?: string | null;
  /** Owner-safe masked value. Public views must not render either field. */
  confirmation_masked?: string | null;
  status?: string;
  starts_at?: string | null;
  ends_at?: string | null;
  start_at?: string | null;
  end_at?: string | null;
  location?: string | null;
  origin?: string | null;
  destination?: string | null;
  service_number?: string | null;
  details?: TravelMetadata | null;
  details_json?: TravelMetadata | string | null;
  created_at?: string;
  updated_at?: string;
  segments?: Segment[];
  [key: string]: unknown;
}

export interface Segment {
  id: string;
  trip_id: string;
  booking_id?: string | null;
  kind: string;
  title?: string | null;
  sequence?: number;
  origin?: string | null;
  destination?: string | null;
  carrier?: string | null;
  service_number?: string | null;
  departs_at?: string | null;
  arrives_at?: string | null;
  start_at?: string | null;
  end_at?: string | null;
  location?: string | null;
  status?: string;
  details?: TravelMetadata | null;
  details_json?: TravelMetadata | string | null;
  created_at?: string;
  updated_at?: string;
  [key: string]: unknown;
}

export interface Receipt {
  id: string;
  account_id?: string | null;
  folder?: string;
  uidvalidity?: number;
  uid?: number;
  message_id?: string | null;
  from_addr?: string | null;
  sender?: string | null;
  subject?: string | null;
  title?: string | null;
  kind?: string | null;
  provider?: string | null;
  confirmation_masked?: string | null;
  booking_status?: string | null;
  parsed_status?: string | null;
  start_at?: string | null;
  end_at?: string | null;
  timezone?: string | null;
  origin?: string | null;
  destination?: string | null;
  decision?: string | null;
  confidence?: number | null;
  excerpt?: string | null;
  body_text?: string | null;
  received_at?: string | null;
  extracted?: TravelMetadata | null;
  extracted_json?: TravelMetadata | string | null;
  trip_id?: string | null;
  status?: string;
  quarantine_reason?: string | null;
  quarantined_at?: string | null;
  created_at?: string;
  updated_at?: string;
  [key: string]: unknown;
}

export interface TravelTask {
  id: string;
  trip_id?: string | null;
  title: string;
  notes?: string | null;
  due_at?: string | null;
  completed_at?: string | null;
  completed?: boolean;
  created_at?: string;
  updated_at?: string;
  [key: string]: unknown;
}

export interface TravelAlert {
  id: string;
  trip_id?: string | null;
  booking_id?: string | null;
  segment_id?: string | null;
  kind: string;
  severity?: string;
  title: string;
  body?: string | null;
  scheduled_at?: string | null;
  triggered_at?: string | null;
  acknowledged_at?: string | null;
  created_at?: string;
  updated_at?: string;
  [key: string]: unknown;
}

/** Share metadata returned to an authenticated owner. Raw tokens are omitted. */
export interface TravelShare {
  id: string;
  trip_id: string;
  token_prefix?: string;
  label?: string | null;
  can_edit_tasks?: boolean;
  expires_at?: string | null;
  revoked_at?: string | null;
  last_accessed_at?: string | null;
  created_at?: string;
  [key: string]: unknown;
}

export interface TravelOverview {
  gmail_onboarding_secure?: boolean;
  settings: TravelSettings | null;
  accounts: Account[];
  trips: Trip[];
  receipts: Receipt[];
  bookings: Booking[];
  segments: Segment[];
  tasks: TravelTask[];
  alerts: TravelAlert[];
  shares: TravelShare[];
  /** Optional compatibility shapes returned by early overview handlers. */
  share_links?: TravelShare[];
  account?: Account | null;
  sync?: { last_synced_at?: string; last_sync_at?: string; [key: string]: unknown } | null;
  generated_at?: string;
  [key: string]: unknown;
}

export interface TravelSettingsInput {
  account_id?: string | null;
  scan_folder?: string;
  home_timezone?: string;
  timezone?: string;
  calendar_name?: string;
  auto_ingest?: boolean;
  default_alert_minutes?: number;
}

export interface TravelSettingsResponse {
  settings: TravelSettings;
  [key: string]: unknown;
}

export interface TravelSyncInput {
  account_id: string;
  folder?: string;
  limit?: number;
  full_rescan?: boolean;
}

export interface TravelSyncResponse {
  status: string;
  folders_scanned?: number | string[];
  messages_examined?: number;
  receipts_imported?: number;
  bookings_created?: number;
  needs_review?: number;
  ignored?: number;
  errors?: unknown[];
  last_sync_at?: string;
  message?: string;
  [key: string]: unknown;
}

export interface ReceiptImportInput {
  account_id?: string;
  from_addr: string;
  subject: string;
  received_at?: string;
  body_text: string;
  html_body?: string;
}

export interface ReceiptImportResponse {
  receipt?: Receipt;
  booking?: Booking | null;
  segments?: Segment[];
  status?: string;
  inserted?: boolean;
  deduped_by?: string | null;
  booking_created?: boolean;
  needs_review?: boolean;
  message?: string;
  errors?: unknown[];
  [key: string]: unknown;
}

export interface TravelActionSummary {
  tone: 'success' | 'warning';
  message: string;
}

/** Turn the sync contract into honest, count-preserving operator feedback. */
export function summarizeTravelSync(result: TravelSyncResponse): TravelActionSummary {
  const status = (result.status || 'complete').toLowerCase();
  const issues = formatIssues(result.errors);
  const failed = ['error', 'failed', 'failure'].includes(status);
  const partial = failed || status === 'partial' || issues.length > 0;
  const counts = [
    typeof result.folders_scanned === 'number'
      ? countPhrase(result.folders_scanned, 'folder scanned', 'folders scanned')
      : undefined,
    countPhrase(result.messages_examined, 'message examined', 'messages examined'),
    countPhrase(result.receipts_imported, 'receipt imported', 'receipts imported'),
    countPhrase(result.bookings_created, 'booking created', 'bookings created'),
    countPhrase(result.needs_review, 'item needs review', 'items need review'),
    countPhrase(result.ignored, 'message ignored', 'messages ignored')
  ].filter((value): value is string => Boolean(value));
  const folders = Array.isArray(result.folders_scanned)
    ? result.folders_scanned.filter((folder): folder is string => typeof folder === 'string')
    : [];
  const details = [
    counts.join(', '),
    folders.length > 0 ? `Folders: ${folders.join(', ')}` : '',
    result.message?.trim() ?? '',
    issues.length > 0 ? `Issues: ${issues.join('; ')}` : ''
  ].filter(Boolean);
  const label = failed ? 'Mailbox check failed' : partial ? 'Mailbox check partial' : 'Mailbox check complete';
  return {
    tone: partial ? 'warning' : 'success',
    message: `${label}${details.length > 0 ? `: ${details.join('. ')}` : '.'}`
  };
}

/** Explain whether a pasted receipt was added, deduplicated, or only reviewed. */
export function summarizeReceiptImport(result: ReceiptImportResponse): TravelActionSummary {
  const status = (result.status || '').toLowerCase();
  const issues = formatIssues(result.errors);
  const failed = ['error', 'failed', 'failure', 'partial'].includes(status) || issues.length > 0;
  if (failed) {
    const details = [
      result.message?.trim() ?? '',
      issues.length > 0 ? `Issues: ${issues.join('; ')}` : ''
    ].filter(Boolean);
    return {
      tone: 'warning',
      message: `Receipt import ${status === 'partial' ? 'partial' : 'failed'}${details.length > 0 ? `: ${details.join('. ')}` : '.'}`
    };
  }
  if (result.inserted === false) {
    return {
      tone: 'success',
      message: `Receipt already on file${result.deduped_by ? ` (matched by ${result.deduped_by.replaceAll('_', ' ')})` : ''}.`
    };
  }
  if (result.needs_review) {
    return {
      tone: 'success',
      message: `Receipt imported for review${result.booking_created ? '; a booking was also added' : ''}.`
    };
  }
  if (result.booking_created || result.booking) {
    return { tone: 'success', message: 'Receipt imported and booking added to the itinerary.' };
  }
  return {
    tone: 'success',
    message: result.message?.trim() || 'Receipt imported. No itinerary booking was created.'
  };
}

function countPhrase(
  value: number | undefined,
  singular: string,
  plural: string
): string | undefined {
  return typeof value === 'number' ? `${value} ${value === 1 ? singular : plural}` : undefined;
}

function formatIssues(value: unknown[] | undefined): string[] {
  if (!Array.isArray(value)) return [];
  const issues = value.slice(0, 4).map((issue) => {
    if (typeof issue === 'string') return issue;
    if (isRecord(issue)) {
      for (const key of ['message', 'error', 'reason', 'code']) {
        if (typeof issue[key] === 'string') return issue[key] as string;
      }
    }
    return 'Unknown sync issue';
  });
  if (value.length > 4) issues.push(`${value.length - 4} more ${value.length === 5 ? 'issue' : 'issues'}`);
  return issues;
}

export interface ReceiptApprovalInput {
  kind?: string;
  title?: string;
  provider?: string | null;
  confirmation_code?: string | null;
  status?: string;
  start_at?: string;
  end_at?: string | null;
  timezone?: string;
  origin?: string | null;
  destination?: string | null;
  address?: string;
  service_number?: string;
}

export interface ReceiptApprovalDraft {
  kind: string;
  title: string;
  provider: string;
  confirmation_code: string;
  status: string;
  start_at: string;
  end_at: string;
  origin: string;
  destination: string;
  clear_provider: boolean;
  clear_confirmation_code: boolean;
  clear_end_at: boolean;
  clear_origin: boolean;
  clear_destination: boolean;
}

/** Build the exact receipt override payload shown in the review form. */
export function buildReceiptApprovalInput(
  draft: ReceiptApprovalDraft
): ReceiptApprovalInput {
  const input: ReceiptApprovalInput = {
    kind: draft.kind.trim(),
    title: draft.title.trim(),
    status: draft.status.trim(),
    start_at: dateTimeInputToIso(draft.start_at)
  };
  const optional: Array<[
    'provider' | 'confirmation_code' | 'end_at' | 'origin' | 'destination',
    string,
    boolean
  ]> = [
    ['provider', draft.provider, draft.clear_provider],
    ['confirmation_code', draft.confirmation_code, draft.clear_confirmation_code],
    ['end_at', dateTimeInputToIso(draft.end_at) ?? '', draft.clear_end_at],
    ['origin', draft.origin, draft.clear_origin],
    ['destination', draft.destination, draft.clear_destination]
  ];
  for (const [key, value, clear] of optional) {
    if (clear) {
      input[key] = null;
      continue;
    }
    const normalized = value.trim();
    if (normalized) input[key] = normalized;
  }
  return input;
}

function dateTimeInputToIso(value: string): string | undefined {
  const normalized = value.trim();
  if (!normalized) return undefined;
  const parsed = new Date(normalized);
  return Number.isNaN(parsed.getTime()) ? normalized : parsed.toISOString();
}

/** True only for an unacknowledged alert that has fired or reached its due time. */
export function isTravelAlertDue(
  alert: Pick<TravelAlert, 'triggered_at' | 'scheduled_at' | 'acknowledged_at'>,
  nowMs = Date.now()
): boolean {
  if (alert.acknowledged_at) return false;
  if (alert.triggered_at) return true;
  if (!alert.scheduled_at) return false;
  const scheduled = Date.parse(alert.scheduled_at);
  return Number.isFinite(scheduled) && scheduled <= nowMs;
}

export interface CreateTripInput {
  title: string;
  destination?: string;
  starts_at?: string;
  ends_at?: string;
  timezone: string;
  status?: string;
  notes?: string;
}

export interface TripResponse {
  trip: Trip;
  [key: string]: unknown;
}

export interface CreateBookingInput {
  trip_id: string;
  kind: string;
  provider?: string;
  title: string;
  confirmation_code?: string;
  status?: string;
  starts_at?: string;
  ends_at?: string;
  location?: string;
  origin?: string;
  destination?: string;
  service_number?: string;
  details?: TravelMetadata;
}

export interface BookingResponse {
  booking: Booking;
  segment?: Segment | null;
  segments?: Segment[];
  [key: string]: unknown;
}

export interface CreateTaskInput {
  title: string;
  notes?: string;
  due_at?: string;
}

export interface TaskResponse {
  task: TravelTask;
  [key: string]: unknown;
}

export interface AlertResponse {
  alert: TravelAlert;
  [key: string]: unknown;
}

export interface CreateShareInput {
  trip_id: string;
  label?: string;
  can_edit_tasks?: boolean;
  expires_at?: string;
}

/** The raw token is revealed once, only by the create-share response. */
export interface CreateShareResponse {
  share: TravelShare;
  token: string;
  url: string;
  calendar_url: string;
  [key: string]: unknown;
}

export interface PublicTravelOverview {
  trip: Trip;
  bookings: Booking[];
  segments: Segment[];
  tasks: TravelTask[];
  alerts?: TravelAlert[];
  calendar_url?: string;
  [key: string]: unknown;
}

// Family views use an explicit second type boundary. Even if the server's
// public response accidentally grows private fields later, only these values
// are retained in page state. Confirmation values, receipts/parser metadata,
// trip/task notes, and alert bodies are intentionally impossible to represent.
export interface SafePublicTrip {
  id: string;
  title: string;
  destination?: string;
  starts_at?: string;
  ends_at?: string;
  timezone?: string;
  status?: string;
}

export interface SafePublicBooking {
  id: string;
  trip_id: string;
  kind: string;
  provider?: string;
  title: string;
  status?: string;
  starts_at?: string;
  ends_at?: string;
  location?: string;
  origin?: string;
  destination?: string;
  service_number?: string;
}

export interface SafePublicSegment {
  id: string;
  booking_id?: string;
  kind: string;
  title?: string;
  origin?: string;
  destination?: string;
  carrier?: string;
  service_number?: string;
  departs_at?: string;
  arrives_at?: string;
  location?: string;
}

export interface SafePublicTask {
  id: string;
  title: string;
  due_at?: string;
  completed_at?: string;
  completed: boolean;
}

export interface SafePublicAlert {
  id: string;
  severity?: string;
  title: string;
  scheduled_at?: string;
  triggered_at?: string;
}

export interface SafePublicTravelOverview {
  trip: SafePublicTrip;
  bookings: SafePublicBooking[];
  segments: SafePublicSegment[];
  tasks: SafePublicTask[];
  alerts: SafePublicAlert[];
  canEditTasks: boolean;
  calendarUrl?: string;
}

/**
 * Project a server response into the only fields the family page may retain.
 * Keep this allowlist narrow; never replace it with object spreads.
 */
export function sanitizePublicTravelOverview(
  raw: PublicTravelOverview
): SafePublicTravelOverview {
  const trip = raw.trip ?? ({ id: '', title: 'Shared trip' } as Trip);
  const root = raw as Record<string, unknown>;
  const permissions = isRecord(root.permissions) ? root.permissions : {};
  return {
    trip: {
      id: asText(trip.id),
      title: asText(trip.title) || 'Shared trip',
      destination: optionalText(trip.destination),
      starts_at: optionalText(trip.starts_at ?? trip.start_at),
      ends_at: optionalText(trip.ends_at ?? trip.end_at),
      timezone: optionalText(trip.timezone),
      status: optionalText(trip.status)
    },
    bookings: list(raw.bookings).map((booking) => ({
      id: asText(booking.id),
      trip_id: asText(booking.trip_id),
      kind: asText(booking.kind) || 'travel',
      provider: optionalText(booking.provider),
      title: asText(booking.title) || titleCase(asText(booking.kind)),
      status: optionalText(booking.status),
      starts_at: optionalText(booking.starts_at ?? booking.start_at),
      ends_at: optionalText(booking.ends_at ?? booking.end_at),
      location: optionalText(booking.location),
      origin: optionalText(booking.origin),
      destination: optionalText(booking.destination),
      service_number: optionalText(booking.service_number)
    })),
    segments: list(raw.segments).map((segment) => ({
      id: asText(segment.id),
      booking_id: optionalText(segment.booking_id),
      kind: asText(segment.kind) || 'travel',
      title: optionalText(segment.title),
      origin: optionalText(segment.origin),
      destination: optionalText(segment.destination),
      carrier: optionalText(segment.carrier),
      service_number: optionalText(segment.service_number),
      departs_at: optionalText(segment.departs_at ?? segment.start_at),
      arrives_at: optionalText(segment.arrives_at ?? segment.end_at),
      location: optionalText(segment.location)
    })),
    tasks: list(raw.tasks).map((task) => ({
      id: asText(task.id),
      title: asText(task.title),
      due_at: optionalText(task.due_at),
      completed_at: optionalText(task.completed_at),
      completed: Boolean(task.completed_at ?? task.completed)
    })),
    alerts: list(raw.alerts).map((alert) => ({
      id: asText(alert.id),
      severity: optionalText(alert.severity),
      title: asText(alert.title),
      scheduled_at: optionalText(alert.scheduled_at),
      triggered_at: optionalText(alert.triggered_at)
    })),
    canEditTasks: Boolean(root.can_edit_tasks ?? permissions.can_edit_tasks),
    calendarUrl: optionalText(root.calendar_url)
  };
}

function list<T>(value: T[] | undefined): T[] {
  return Array.isArray(value) ? value : [];
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return Boolean(value) && typeof value === 'object' && !Array.isArray(value);
}

function asText(value: unknown): string {
  return typeof value === 'string' ? value : '';
}

function optionalText(value: unknown): string | undefined {
  const result = asText(value);
  return result || undefined;
}

function titleCase(value: string): string {
  return (value || 'travel')
    .replaceAll('_', ' ')
    .replace(/\b\w/g, (letter) => letter.toUpperCase());
}

export const GMAIL_SMTP_HOST = 'smtp.gmail.com' as const;
export const GMAIL_SMTP_PORT = 587 as const;
export const GMAIL_IMAP_HOST = 'imap.gmail.com' as const;
export const GMAIL_IMAP_PORT = 993 as const;

export interface GmailConnectInput {
  email: string;
  /** Sent only in the atomic POST /api/travel/connect-gmail request. */
  password: string;
  display_name?: string;
  /** Non-secret travel preferences saved only after IMAP verification passes. */
  settings?: Omit<TravelSettingsInput, 'account_id'>;
}

export interface GmailConnectResult {
  account: Account;
  verification: VerifyResult;
  /** Null when Gmail rejected the credential; that account is not selected. */
  settings: TravelSettings | null;
}

/** Keep caller options from changing a helper's HTTP verb, body, or query. */
function transportOptions(o?: RequestOptions): Pick<RequestOptions, 'fetchImpl' | 'signal'> {
  return { fetchImpl: o?.fetchImpl, signal: o?.signal };
}

/**
 * Capability-token mutations sit outside dashboard auth and CSRF. Keep them
 * independent from the owner request client so a family member never has to
 * mint a protected dashboard token. JSON content type also prevents a
 * cross-site HTML form from issuing a simple request with the share token.
 */
async function publicMutation<T>(
  path: string,
  o?: RequestOptions
): Promise<T> {
  const fetchImpl = o?.fetchImpl ?? fetch;
  const response = await fetchImpl(`/api${path}`, {
    method: 'POST',
    headers: { Accept: 'application/json', 'Content-Type': 'application/json' },
    credentials: 'omit',
    cache: 'no-store',
    signal: o?.signal,
    body: '{}'
  });
  if (!response.ok) {
    let body: { code?: string; message?: string; error?: string } | undefined;
    try {
      body = (await response.clone().json()) as typeof body;
    } catch {
      body = undefined;
    }
    throw new EnvelopeApiError(
      response.status,
      body?.code ?? `http_${response.status}`,
      body?.message ?? body?.error ?? `request failed (${response.status})`,
      body
    );
  }
  return (await response.json()) as T;
}

export const travelApi = {
  overview(o?: RequestOptions): Promise<TravelOverview> {
    return request('/travel/overview', { ...transportOptions(o), method: 'GET' });
  },

  saveSettings(
    settings: TravelSettingsInput,
    o?: RequestOptions
  ): Promise<TravelSettingsResponse> {
    return request('/travel/settings', {
      ...transportOptions(o),
      method: 'PUT',
      body: settings
    });
  },

  sync(input: TravelSyncInput, o?: RequestOptions): Promise<TravelSyncResponse> {
    return request('/travel/sync', {
      ...transportOptions(o),
      method: 'POST',
      body: input
    });
  },

  importReceipt(
    input: ReceiptImportInput,
    o?: RequestOptions
  ): Promise<ReceiptImportResponse> {
    return request('/travel/import', {
      ...transportOptions(o),
      method: 'POST',
      body: input
    });
  },

  approveReceipt(
    receiptId: string,
    input: ReceiptApprovalInput = {},
    o?: RequestOptions
  ): Promise<ReceiptImportResponse> {
    return request(`/travel/receipts/${encodeURIComponent(receiptId)}/approve`, {
      ...transportOptions(o),
      method: 'POST',
      body: input
    });
  },

  dismissReceipt(receiptId: string, o?: RequestOptions): Promise<ReceiptImportResponse> {
    return request(`/travel/receipts/${encodeURIComponent(receiptId)}/dismiss`, {
      ...transportOptions(o),
      method: 'POST'
    });
  },

  createTrip(input: CreateTripInput, o?: RequestOptions): Promise<TripResponse> {
    return request('/travel/trips', {
      ...transportOptions(o),
      method: 'POST',
      body: input
    });
  },

  createBooking(input: CreateBookingInput, o?: RequestOptions): Promise<BookingResponse> {
    return request('/travel/bookings', {
      ...transportOptions(o),
      method: 'POST',
      body: input
    });
  },

  createTask(
    tripId: string,
    input: CreateTaskInput,
    o?: RequestOptions
  ): Promise<TaskResponse> {
    return request(`/travel/trips/${encodeURIComponent(tripId)}/tasks`, {
      ...transportOptions(o),
      method: 'POST',
      body: input
    });
  },

  toggleTask(taskId: string, o?: RequestOptions): Promise<TaskResponse> {
    return request(`/travel/tasks/${encodeURIComponent(taskId)}/toggle`, {
      ...transportOptions(o),
      method: 'POST'
    });
  },

  acknowledgeAlert(alertId: string, o?: RequestOptions): Promise<AlertResponse> {
    return request(`/travel/alerts/${encodeURIComponent(alertId)}/acknowledge`, {
      ...transportOptions(o),
      method: 'POST'
    });
  },

  createShare(input: CreateShareInput, o?: RequestOptions): Promise<CreateShareResponse> {
    return request('/travel/shares', {
      ...transportOptions(o),
      method: 'POST',
      body: input
    });
  },

  deleteShare(
    shareId: string,
    o?: RequestOptions
  ): Promise<{ deleted?: string; share?: TravelShare; status?: string }> {
    return request(`/travel/shares/${encodeURIComponent(shareId)}`, {
      ...transportOptions(o),
      method: 'DELETE'
    });
  },

  /**
   * Atomically verify Gmail over IMAP, persist the account, and select it for
   * travel ingestion. The backend commits nothing when verification fails.
   *
   * The app password is placed directly in this one request body. It is not
   * copied into settings, returned, logged, or written to browser storage.
   */
  async connectGmail(input: GmailConnectInput, o?: RequestOptions): Promise<GmailConnectResult> {
    return request<GmailConnectResult>('/travel/connect-gmail', {
      ...transportOptions(o),
      method: 'POST',
      body: {
        email: input.email,
        password: input.password,
        display_name: input.display_name,
        settings: input.settings
      }
    });
  }
};

export const publicTravelApi = {
  overview(token: string, o?: RequestOptions): Promise<PublicTravelOverview> {
    return request(`/public/travel/${encodeURIComponent(token)}`, {
      ...transportOptions(o),
      method: 'GET',
      cache: 'no-store'
    });
  },

  toggleTask(token: string, taskId: string, o?: RequestOptions): Promise<TaskResponse> {
    return publicMutation(
      `/public/travel/${encodeURIComponent(token)}/tasks/${encodeURIComponent(taskId)}/toggle`,
      o
    );
  },

};
