# Private access with Tailscale

Travel works behind Tailscale Serve on a personal Mac or Linux server. Tailscale
is optional and user-operated; this does not require public hosting or Funnel.

1. Install Tailscale on the host and each family device, sign in, and configure
   your tailnet access rules to allow only the intended household devices/users.
2. Set a strong, persistent `TRAVEL_TOKEN` in the service's protected environment
   file, then restart Travel bound to `127.0.0.1:3150`. Do not put secrets in
   this repository. For systemd use the supplied `travel.service`; for a
   container publish only `127.0.0.1:3150:3150` and run Tailscale on the host.
3. Inspect existing routes first with `tailscale serve status`. Use a free
   HTTPS port if this machine already serves another application. Then run:

   ```sh
   tailscale serve --bg --https=8443 http://127.0.0.1:3150
   tailscale serve status
   ```

   Follow Tailscale's HTTPS enablement prompt if needed. Open the displayed
   HTTPS address with `/travel` appended, including port 8443. The background
   route persists across Tailscale restarts; Travel must also be running.
4. Sign in on Connections with the owner token. Create scoped household
   invitations for relatives; do not give them the owner token. Test from a
   second tailnet device: unauthenticated private API requests must return 401,
   while invitation-based access must respect its trip and role.

Keep Travel's application authentication enabled even on your tailnet. This
standalone build deliberately does not translate Tailscale identity headers into
owner permissions. Network membership is not a household role.

Use the HTTPS origin for companion pairing and calendar subscriptions. A client
must be able to reach the tailnet to refresh feeds. Cloud calendar providers
generally cannot fetch private tailnet URLs; use a device-local subscription or
download an `.ics` file instead. A sleeping laptop remains unavailable even when
Tailscale is configured. Web Push still uses browser-vendor transport if enabled.

To remove only this route (without resetting other services):

```sh
tailscale serve --https=8443 off
```

Do not use `tailscale funnel`: it makes an endpoint publicly accessible.
Tailscale uses its configured coordination/relay services; HTTPS certificates
also involve a certificate authority and certificate transparency logs. Choose
non-sensitive machine names. No tailnet credentials belong in the Travel image.

References: [Serve](https://tailscale.com/docs/features/tailscale-serve),
[Serve CLI](https://tailscale.com/docs/reference/tailscale-cli/serve).
