# Development

[← Back to the README](../README.md)

This part is for programmers who want to dive into the code.

## How it works

**Talking to a display (MDC).** Everything after the first setup goes through Samsung's MDC
protocol, over the network:

1. `emdx` connects to the display on TCP port 1515; the display greets with
   `MDCSTART<<TLS>>`.
2. A TLS session starts (the display's certificate is self-signed), `emdx` sends the PIN
   and the display answers `MDCAUTH<<PASS>>` or `MDCAUTH<<FAIL:…>>`.
3. Commands are frames `AA cmd id len data… checksum`. Most settings use a sub-command as
   the first data byte (for example `C8 81 01`: orientation, portrait). A few commands use
   "large" frames with a 2-byte length. The display replies `AA FF id len A|N cmd data…
   checksum`, `A` for ACK and `N` for NAK.
4. The EM32DX accepts **one session at a time** and drops replies to back-to-back sessions,
   so `emdx` sends commands one after another (`emdx serve` keeps one lock per display).

**Showing an image.** `emdx` does what the Android app does. It starts a small HTTPS server
(port 6869), hands the display its self-signed certificate (`D2 20`), then sends MDC `C7 53`
with the URL of a JSON manifest, `https://<you>:6869/content?id=…&content_type=…`. The display
downloads the manifest, then each image from `/image?path=…`, and may report its progress to
`/content-transfer-progress`; then it refreshes. The manifest is the app's, key for key: a
single image, a slideshow (`PlaylistContent`) and a schedule (`ScheduleContent`) differ only in
its `schedule` blocks. The *mobile* content player must be selected for the display to show it;
selecting it is the one step `emdx` adds to what the app sends.

**Seeing what is on the screen (thumbnails).** The other direction of the same trick: `emdx`
starts a receiver, pushes its self-signed certificate (`D2 20`, as the app does) and sends `C7 54`
with the URL `https://<you>:6869/device-thumbnail`. Over plain HTTP (`http://<you>:6868/…`, the
default of `emdx thumbnail` without `--secure`) the display acknowledges but never uploaded in our tests.
The display answers with a `multipart/form-data` POST — the JPEG plus `device_id` (its serial),
`content_id`, `content_name`, `content_player_type` and timestamps — which `emdx` saves per serial.
A display posts one on its own whenever what it shows changes, so `emdx serve` listens all the
time rather than only while asked. Reverse-engineered from the Samsung app.

**First-time setup (Bluetooth LE).** A display in setup mode advertises a GATT service whose
characteristic carries framed commands (2-byte header: id, fragment, more, operation,
length; long payloads are split into chunks).
1. `emdx` connects over LE only (BlueZ `ConnectDevice`; a plain connect may pick
   Bluetooth Classic on this dual-mode display).
2. The display starts the pairing itself (LE Secure Connections, "Just Works").
3. `emdx` authorizes with the PIN (a bcrypt challenge), then sends the network settings.
4. Once the display reports its IP address, the rest of the setup runs over MDC.

## Code map

| Path | What it does |
| --- | --- |
| `src/bin/emdx/main.rs` | CLI commands and their dispatch; image push (`push_image`) |
| `src/bin/emdx/thumbnail.rs` | `emdx thumbnail`: start the receiver, ask, save, open |
| `src/bin/emdx/setup.rs`, `config.rs` | `emdx add` (displays already on the network), the saved-displays file |
| `src/bin/emdx/provision.rs` | `emdx setup` (first-time setup over Bluetooth, with the settings questions) |
| `src/bin/emdx/settings.rs` | settings, forget, reset, pairing mode (shared with the web server) |
| `src/bin/emdx/serve.rs`, `ui/` | `emdx serve`: JSON API under `/api`, and the UI (plain HTML, CSS, JavaScript) |
| `src/bin/emdx/saved_slideshow.rs` | the web editor's slideshows kept on disk per display, so they can be re-edited |
| `src/bin/emdx/wifi.rs` | the Wi-Fi networks around this computer (for the setup form), via the `wifi_scan` crate |
| `src/bin/emdx/fanout.rs` | `--all`: run one action on every saved display concurrently |
| `src/mdc.rs` | MDC protocol: TLS session, framing, every command, reply parsers |
| `src/thumbnail.rs` | the `POST /device-thumbnail` receiver, the upload, where thumbnails are saved |
| `src/cms.rs` | sending content like the app's server: HTTPS, `/content`, `/image`, progress reports |
| `src/content.rs` | the manifest, in the Android app's exact JSON shape |
| `src/slideshow.rs` | slideshows (the app's playlists) |
| `src/schedule.rs` | schedules: one manifest block per start time |
| `src/ble/` | Bluetooth setup protocol (`proto.rs`, `session.rs`) and the BlueZ transport (`bluez.rs`) |
| `src/provision.rs` | the setup flow (Bluetooth phase, then MDC phase) |
| `src/log.rs` | progress lines (`say!`), the `-v` / `--quiet` filters, logcat formatting and `--log-file` |
| `src/placeholder.rs` | the identification image (text rendering with an embedded font) |
| `src/discovery.rs`, `wol.rs`, `imaging.rs`, `local_ip.rs` | SSDP discovery and MAC lookup, Wake-on-LAN, JPEG handling, picking the local IP |

## Adding a display command

A new setting usually takes this path:
1. **`src/mdc.rs`:** add the command and sub-command bytes to `cmd`/`sub`, a reply parser
   (a pure function, with a unit test), and getter/setter methods on `Session`.
2. **`src/bin/emdx/main.rs` (+ `settings.rs`):** a CLI command that shows the current value
   when called without one.
3. **`src/bin/emdx/serve.rs`:** add it to the status JSON and to `PUT …/settings` (validate the
   value *before* connecting to the display).
4. **`src/bin/emdx/ui/app.js`:** a row in the Settings card.
5. **Tests:** a parser test in `mdc.rs` and, for the exact bytes sent, a test against the mock
   display in `tests/mdc_session.rs`.

## Tests

`cargo test` runs everything; no display or Bluetooth adapter is needed.
- **Unit tests** in each module cover the MDC protocol (framing, checksums, auth, settings
  and identity decoding), the Bluetooth setup protocol (with real data captured from a
  display and from the phone app), Wake-on-LAN, SSDP/ARP parsing, IP selection, the
  identification image, and the web API (against a temporary config).
- **`tests/common/`** is a mock display: it speaks MDC over real TLS, records the commands it
  receives and, on a content-download command, fetches the manifest and images like the real
  panel (over HTTPS it trusts only the certificate it was pushed). On a thumbnail request (`C7 54`) it posts a multipart upload back to the URL it
  was given, exactly like the real panel does.
- **`tests/mdc_session.rs`** runs the MDC client against it: auth, framing, NAKs, timeouts,
  and the bytes of the settings and reset commands.
- **`tests/cms.rs`**, **`tests/cms_plain_http.rs`**, **`tests/slideshow.rs`** and
  **`tests/schedule.rs`** send an image, a slideshow and a schedule end to end (HTTPS with the
  pushed certificate, and the plain-HTTP fallback), and check the manifest the display fetches.
- **`tests/thumbnail.rs`** runs the whole thumbnail exchange against it: the frame that is sent,
  the upload that comes back, a display that refuses to answer, and watch mode.
- **`tests/provision.rs`** runs the setup flow against it and checks the order of the steps.
