import { describe, expect, it } from 'vitest';

const TRAVEL_SOURCE = Object.values(
  import.meta.glob('../routes/travel/+page.svelte', {
    query: '?raw',
    import: 'default',
    eager: true
  })
)[0] as string;

describe('travel page audit contracts', () => {
  it('keeps the exact Gmail scan folder editable during onboarding and in settings', () => {
    expect(TRAVEL_SOURCE).toContain('bind:value={onboardingScanFolder}');
    expect(TRAVEL_SOURCE).toContain('bind:value={settingsScanFolder}');
    expect(TRAVEL_SOURCE).toContain('[Gmail]/All Mail');
    expect(TRAVEL_SOURCE).toContain("scan_folder: onboardingScanFolder.trim() || 'INBOX'");
    expect(TRAVEL_SOURCE).toContain("scan_folder: settingsScanFolder.trim() || 'INBOX'");
  });

  it('uses result-aware sync/import feedback and labels refresh time honestly', () => {
    expect(TRAVEL_SOURCE).toContain('summarizeTravelSync(result)');
    expect(TRAVEL_SOURCE).toContain('summarizeReceiptImport(result)');
    expect(TRAVEL_SOURCE).toContain('View refreshed');
    expect(TRAVEL_SOURCE).not.toContain('<dt>Last checked</dt>');
    expect(TRAVEL_SOURCE).toContain('<dt>Last mailbox check</dt>');
  });

  it('submits editable receipt overrides instead of an empty approval body', () => {
    for (const field of [
      'edit.kind',
      'edit.title',
      'edit.provider',
      'edit.confirmation_code',
      'edit.status',
      'edit.start_at',
      'edit.end_at',
      'edit.origin',
      'edit.destination'
    ]) {
      expect(TRAVEL_SOURCE).toContain(`bind:value={${field}}`);
    }
    for (const field of [
      'edit.clear_provider',
      'edit.clear_confirmation_code',
      'edit.clear_end_at',
      'edit.clear_origin',
      'edit.clear_destination'
    ]) {
      expect(TRAVEL_SOURCE).toContain(`bind:checked={${field}}`);
    }
    expect(TRAVEL_SOURCE).toContain('Extracted {receipt.confirmation_masked}. Leave blank to keep it.');
    expect(TRAVEL_SOURCE).toContain('buildReceiptApprovalInput(draft)');
    expect(TRAVEL_SOURCE).not.toMatch(/approveReceipt\(receipt\.id\s*\)/);
  });

  it('hides Gmail secret entry when the backend marks the connection unsafe', () => {
    expect(TRAVEL_SOURCE).toContain('overview?.gmail_onboarding_secure !== false');
    expect(TRAVEL_SOURCE).toContain('{#if gmailOnboardingSecure}');
    expect(TRAVEL_SOURCE).toContain('HTTPS or Tailscale Serve');
  });
});
