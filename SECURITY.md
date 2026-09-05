# Security and responsible use

Travel is a development preview. It has not completed a comprehensive security
audit or the full product acceptance plan. Do not expose an unauthenticated
instance or rely on it as your sole source of travel deadlines.

- Use a unique `TRAVEL_TOKEN` and HTTPS for remote access, including proxies
  bound to a local listener. Use one isolated instance per household.
- Keep credentials in the configured credential store or environment references.
  Do not put secrets in model arguments, URLs, issues, screenshots, or logs.
- Custom model CLIs currently require explicit unsandboxed opt-in. Only use
  executables you trust. Receipts are untrusted input.
- Private calendars, source documents, backups, and reservation management URLs
  contain personal information. Do not commit or publicly share them.
- Public family links are bearer capabilities: anyone possessing one can access
  its scope. Revoke links that may have been disclosed.
- Browser push is optional and uses browser-vendor infrastructure. Provider
  acceptance is not user acknowledgment or a delivery guarantee.

For a suspected vulnerability, use GitHub's private vulnerability reporting on
this repository. Do not open a public issue containing exploitable details,
credentials, or personal receipts. Use synthetic fixtures for ordinary bug reports.

The bundled web interface is static; Node.js and SvelteKit's server are not
production request handlers. Dependency scans should still cover build tools
as well as the Rust runtime. No claim of being vulnerability-free is made.
