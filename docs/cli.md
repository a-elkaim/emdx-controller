# Command line

[← Back to the README](../README.md)

## Set up a fresh display (new, or factory reset)

A display that has never been set up (or was factory reset) shows its setup screen: the
**pairing PIN** is the 6-digit number under the bottom QR code (514395 here). It isn't on
any network yet: `emdx` connects to it over Bluetooth, gives it your network, then
finishes the setup over the network. See the Bluetooth requirements in
[Requirements](build.md#requirements).

> [!TIP]
> If `emdx setup` finds no display, put it in **pairing mode**: press the Bluetooth button on
> the back of the screen; the LED blinks blue. If nothing happens, the display is asleep:
> press its power button first (the LED turns green while it wakes up), then the Bluetooth
> button; the LED should blink blue.

<p align="center"><img src="01%20-%20setup%20screen.jpeg" alt="Setup screen of a new display: QR codes, and the pairing PIN under the bottom one" width="320"></p>

> [!NOTE]
> The pairing PIN is temporary (a new one appears each time the display enters setup mode)
> and only serves for this setup. Afterwards, the display uses its **display PIN**: the one
> you choose during the setup (`--display-pin`), or, if you don't choose one, the pairing PIN
> shown at that moment.
>
> A display that **was set up before** (put back in pairing mode with `emdx pairing-mode`,
> `emdx forget` or its button) shows no PIN: its pairing PIN is its **display PIN**.

```bash
emdx setup --list              # displays seen over Bluetooth (setup mode or not)
emdx setup lobby               # interactive: display, PINs, network, settings, terms
emdx setup lobby --pairing-pin 123456 --wifi MyWifi       # asks for the Wi-Fi password
emdx setup lobby --pairing-pin 123456 --wifi MyWifi --wifi-password secret   # no prompts
emdx setup lobby --pairing-pin 123456 --wifi MyWifi --orientation portrait \
    --sleep-time 20 --network-standby on --daily-refresh 03:00 --display-pin 654321
```

- **Network:** Wi-Fi (the display only joins **2.4 GHz** networks); its address comes from
  your router (DHCP).
- **Terms & conditions:** the display offers Samsung's terms; `emdx` shows their URL and
  only accepts them if you say so (prompt or `--accept-terms`). `--decline-terms` refuses
  them without asking. Declining them doesn't stop `emdx` from showing images.
- **Display settings** (the ones the phone app asks at the end of its setup): orientation
  (`--orientation landscape|portrait`), sleep time (`--sleep-time`), network standby
  (`--network-standby on|off`) and screen protection time (`--daily-refresh HH:MM`). In
  interactive mode they are asked at the start; Enter keeps the display's own setting.
- **Display PIN** (required): the PIN the display asks from now on (`--display-pin`, or asked
  twice, hidden). The pairing PIN is only for the setup.
- **Name:** the display is named on the display itself like it is saved in emdx;
  `--device-name` sets another on-display name. `--no-placeholder` skips the identification
  image.

What happens:
1. **Bluetooth:** `emdx` connects; the display starts the pairing itself (no code to
   confirm). `emdx` authorizes with the pairing PIN, sends the network settings and waits for the
   display's address.
2. **Network:** it sets the clock, handles the terms, applies the display settings and the
   display PIN, selects the *mobile* content player (needed to show images sent by `emdx`) and
   marks the setup as complete.
3. **Saved:** the display is saved under the name you gave, so `emdx -d lobby status`
   works right away.
4. **Identification:** the display shows its name, IP, Wi-Fi network, MAC, serial and
   software version.

## Add a display that is already set up (needs its display PIN)

> [!NOTE]
> This needs the **display PIN**, the one chosen during the display's setup, not the
> temporary pairing PIN shown on a setup screen.

```bash
emdx add                        # interactive: scan the network, pick, name, display PIN
emdx add --scan                 # straight to scanning
emdx add lobby --manual         # prompts for IP and display PIN
emdx add office --host 192.168.1.35 --display-pin 123456              # no prompts (scripts)
emdx add office --host 192.168.1.35 --display-pin 123456 --default    # and make it the default
emdx discover                   # list the displays found on the network (and update moved ones)
```

**Displays that get a new IP address.** emdx remembers each display's MAC. When a saved
display doesn't answer at its saved address, emdx looks for it on the network (SSDP, 3 s);
if the same MAC answers at another address, the address is updated and saved, then the
command goes on. `emdx discover` and **Search the network** in `emdx serve` update moved
displays the same way. The display has to be awake to answer; a display saved without a
MAC (re-add it, or set it in EMDX Settings) can't be followed.

The interactive mode lists the displays it finds (marking ones already saved), suggests a
name, asks for the display PIN and checks it by connecting before saving. The MAC address is
detected automatically when the display is on the same local network; across a router,
`add` asks for it (optional; only `wake` needs it). `--mac` always overrides it.

The first time you run any command with no display saved, `add` starts by itself.

## Show an image

```bash
emdx show-image --image photo.jpg                   # on the default display
emdx -d lobby show-image --image photo.png --sleep-after 20   # power off 20 min later
emdx -d lobby identify                              # its name and details, on the display
emdx -d lobby identify -o card.jpg                  # …or save that image to a file
```

JPEG, PNG, WebP, GIF and BMP are supported. The display only shows images from emdx with
the *mobile* content player, so emdx switches to it first when another one is selected.

Images are sent the way the Android app sends them: over HTTPS on port 6869, with a
certificate handed to the display first. Your firewall must let the display reach that port
(a spare one is used when it is taken). `--content-http` (or `EMDX_CONTENT_HTTP=1`) uses plain
HTTP on port 6868 instead, like the app with its "Secure Mobile CMS" setting off; it applies
to `show-image`, `slideshow`, `schedule` and `emdx serve`.

A **slideshow** shows images in turn, each for the same time (5 minutes to 24 hours,
as in the app's playlists), in the order given. The display downloads them all, then runs it
on its own:

```bash
emdx -d lobby slideshow --image a.jpg --image b.png --image c.webp --every 30   # every 30 min
emdx -d lobby slideshow --image a.jpg --image b.jpg --every 4h                  # or 1:30, 1h30
```

A **schedule** changes the image at set times of day, every day (the app's schedules). Each
`--at` is a start time and the image shown from then until the next one; as in the app, there
is no duration to set. The display downloads everything, then runs it on its own:

```bash
emdx -d lobby schedule --at 08:00 morning.jpg --at 12:00 lunch.jpg --at 18:30 evening.jpg
```

A start time can be used once. What the display shows before the first start time of the day
has not been checked on a real display yet: start one entry at `00:00` to be sure.

## Ask the display what it is showing

A display never sends its picture on request — it *uploads* it. `emdx` starts a small receiver,
invites the display to post its thumbnail there (MDC command `0xC7`, sub-command `0x54`), and
saves what arrives:

```bash
emdx thumbnail                     # ask, wait for it, save it, print the path
emdx -d lobby thumbnail --open     # …and open it
emdx thumbnail --watch             # keep listening: a display posts one whenever its screen changes
emdx thumbnail --listen-only       # only the receiver, no display configured
emdx thumbnail --rotate none --out /tmp/shots   # as sent (default: auto, upright from the orientation)
emdx thumbnail --secure            # like the app: push our certificate (0xD2/0x20), receive over HTTPS on 6869
```

The MDC session stays open until the upload arrives (the app keeps its socket open too);
`--no-hold` closes it right after the display acknowledges.

Files land in `<config dir>/thumbnails/<serial>/thumbnail_<uuid>.jpg`, i.e. next to your saved
displays (`~/.config/emdx/thumbnails/…`), or anywhere you choose with `--out <dir>`. Thumbnails are matched to a display by its
serial number, which `emdx status` reads and remembers.

This is the same mechanism the Samsung app uses.

## Check the status, wake it up or put it to sleep

```bash
emdx status                 # battery, identity, network, current settings
emdx wake                   # Wake-on-LAN
emdx sleep                  # deep sleep: only its power button wakes it (not Wake-on-LAN)
emdx status --all           # every saved display, in parallel (also: wake --all, sleep --all)
```

A sleeping display drops off the network. The other commands wake it with Wake-on-LAN
when it doesn't answer, which needs its MAC address (found automatically in most cases).

## Change the display settings

Each command shows the current value when called without one.

```bash
emdx content-player mobile      # mobile (images from emdx), usb, vxt, custom-app
emdx orientation portrait       # landscape or portrait
emdx background-color green     # the six the app offers: white, black, red, green, blue, yellow (no custom colour)
emdx sleep-time 20              # stay awake 20 min after new content: off, minutes or 1:30 (5 min to 23:59)
emdx sleep-after-update on      # sleep as soon as new content is shown
emdx change-pin                 # change the display PIN on the display (asked twice), then save it
emdx reset defaults             # settings back to default and stored content deleted (the app's "Reset All")
emdx daily-refresh 23:00        # screen protection time: refresh at 23:00 (every 3 days per the app; `now`: refresh now)
emdx network-standby on         # stay reachable while asleep (uses more battery)
emdx sleep-mode deep-sleep      # as in the app: sleep, deep-sleep (only the power button wakes it) or off
```

## Manage the saved displays

```bash
emdx displays                   # list them (* = default)
emdx displays default lobby
emdx displays rename office hall
emdx displays edit lobby --host 192.168.1.60 --pin 654321 --mac ""   # change the saved address/PIN/MAC (local only; use change-pin to change it on the display)
emdx displays remove hall       # forget it in emdx only; the display is unchanged
```

Each display is saved under a short name with its IP address, display PIN and MAC
address. Commands use the **default** one (the first saved, or the one you choose), or
another with `-d <name>` (or `EMDX_DISPLAY`). The list is stored in
`~/.config/emdx/config.json` (or `$XDG_CONFIG_HOME/emdx/`, or the path in `EMDX_CONFIG`),
readable only by you since it holds PINs, and is shared with the web interface.

## Forget, re-pair or reset a display

```bash
emdx forget -d lobby                  # settings back to default, content deleted ("Reset All"), removed from emdx
emdx pairing-mode -d lobby            # back to Bluetooth pairing mode only (LED blinks blue)
emdx reset settings -d lobby          # the app's "Reset Settings"
emdx reset all -d lobby               # the app's "Reset All"
emdx empty-storage -d lobby           # the app's "empty storage" (or --what network-certs|app-certs)
```

Each one asks for confirmation (`--yes` skips it). `forget` removes nothing if the display
can't be reached, and it prints the display PIN before removing it: you need it to set the
display up again (see the note in [Set up a fresh display](#set-up-a-fresh-display-new-or-factory-reset)).

What each one did on an EM32DX (software S-RSEDWWC-1050.1):

| | Settings | Content | Network, display PIN, "set up" state | Pairing mode |
| --- | --- | --- | --- | --- |
| `reset settings` | only the sleep time back to 20 min | kept | kept | no |
| `empty-storage` | unchanged | the image on screen stayed | kept | no |
| `reset all` | content player back to **USB**, screen protection time 01:00, sleep time 20 min | erased (stock wallpaper, after a while) | **kept** | no |
| `pairing-mode` | unchanged | kept | kept | yes (LED blinks blue, no PIN shown) |

So "Reset All" is not a full factory reset. Sending an image afterwards switches the content
player back to *mobile* by itself; run `emdx pairing-mode` to set the display up again.
`emdx reset defaults` is the same as `emdx reset all`, and is what **Reset to default
settings** in `emdx serve` sends. A display
reset to its factory settings from its own menu shows the setup screen with a new pairing
PIN instead.

## One-off use without saving

Flags always win over saved values:

```bash
emdx status --host 192.168.1.34 --display-pin 123456
emdx show-image --host 192.168.1.34 --display-pin 123456 --image photo.jpg [--mac …]
emdx wake --mac b0:f2:f6:55:0d:0b
```

## Logs

Everything (progress lines and diagnostics) goes to **stderr**, so a command's own
output stays on stdout. `-v` / `-vv` / `-vvv` raise the diagnostics to info / debug /
trace, `-q` hides the progress lines, and `--log-file <path>` appends everything to a
file you can attach to a bug report.

Lines are formatted like Android logcat's `threadtime` — date, time, pid, thread,
one-letter level (V dim, D cyan, I green, W yellow, E red) and a tag (the module that
logged the line, `emdx` for the progress lines) — on the console and in the file:

```text
11-23 12:25:19.938  1234  1234 I emdx: 🔄 Connecting to "em32dx-743d"…
11-23 12:25:20.033  1234  1234 V mdc: MDC → command=11 frame=AA 11 00 00 11
11-23 12:25:20.037  1234  1234 D mdc: MDC command failed command=11 error=NAK …
```

Each run starts the log file with a `--------- beginning of emdx log … ---------` line,
which separates one run from the next in an appended file. `emdx serve` also prints it
on the console.

The same settings work from the environment: `EMDX_LOG_FILE`, and `EMDX_LOG` for the
filters (`EMDX_LOG=emdx_controller::mdc=trace` traces one module's frames).
