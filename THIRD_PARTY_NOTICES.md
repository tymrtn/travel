# Third-party notices

Travel derives its Rust storage/mail libraries and initial interface from
[Envelope](https://github.com/tymrtn/envelope-email), by Tyler Martin, under the
FSL-1.1-ALv2 license retained in this repository. Some internal package and API
names retain that heritage; an installed Envelope service is not required.

The browser bundle includes Instrument Sans and DM Mono under the SIL Open Font
License. Their original notices are in `licenses/` and in the built browser
assets' `licenses/` directory.

Rust and JavaScript dependencies retain their respective licenses. Exact
dependencies are recorded in `Cargo.lock` and
`crates/dashboard/web/package-lock.json`. Building from source obtains them
from their package registries. The optional macOS packager also copies OpenSSL's
license alongside its bundled libraries.
