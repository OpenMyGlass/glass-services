# Wi-Fi setup without a companion: access point and page

Status: code in `crates/glassd/src/wifi_setup/`, host-tested, **not yet run on a Glass**. Launcher UI not built (section 6).

## 1. Goal

An owner with no companion gives the Glass a Wi-Fi network: the Glass opens its own access point for a few minutes, shows its name and a random password, and a page it serves takes the network's name and password. Off by default.

## 2. Is AP mode possible on this firmware and kernel?

Yes, with one trick. Experiments on the Glass on 2026-11-05, kernel 3.4.113, firmware 5.90.195.122, hostapd 2.11 (Alpine). Logs are in `out/` of the working tree, scripts in `.build/exp-ap-ble` (neither is in git).

| Evidence | Result |
|---|---|
| `kmsg_ap1`, firmware path unchanged | `Firmware up: op_mode=0x0005`. hostapd and wpa_supplicant both log `AP-ENABLED`. Per the 2026-11-05 notes this is false: nothing could join. No phone log of that attempt survives, so read that as a note and not a measurement |
| `kmsg_ap4`, `ap4_phone.txt`, channel 1, `firmware_path` a symlink named `*_apsta*` | `op_mode=0x0002`. Phone joined with WPA2, took a lease, 50/50 pings at 15.8 ms, 8 MB transfers both ways (Glass to phone MD5 matches; phone to Glass the same MD5 in 3 runs) |
| `kmsg_ap5`, `ap5_phone.txt`, channel 11, same way | Same result: phone joined and took a lease. 15.7 ms |
| `kmsg_ap3`, `ap3_phone.txt`, same way | **Phone never joined** in 140 s: it dropped its home network for about 10 s, went back, and the log has no `AP-STA-CONNECTED` and no lease. Cause not established. So far the phone joined in 2 of 3 runs |
| `kmsg_ap1`, after an `apsta` run, `echo "" > firmware_path`, next link-up | Back to `op_mode=0x0005` |

Why the symlink works: `dhd_get_fw_mode` (`drivers/net/wireless/bcmdhd/dhd_linux.c`) picks the mode from the substring `_apsta` in the firmware's path. The firmware file is the same one. The parameter is consumed when the firmware loads, so it is written just before the link-up.

The Wi-Fi being 2.4 GHz only (`kmsg_wifi29.txt`) is no obstacle to the access point: `iw phy` lists only 2.4 GHz channels, and AP among the interface modes. It is a limit on the network the owner can give, so the page says so. A 5 GHz network ends in a failed join.

Not established by any log:
- the same sequence driven by glassd (`rfkill`, `ip`, and no `country_code`, where the experiments set one);
- restoring the normal firmware and then joining in one boot, end to end;
- a phone with mobile data on (the test phone has no SIM), see 7.1.

These are in the test plan (section 8).

## 3. Lifecycle

`WifiSetup` is a state machine run from the daemon's `poll` loop. No wait blocks the loop, except the link-up that loads the firmware (about 1 s), as for any Wi-Fi on.

```
Off -> Settling -> Starting -> Serving -> Flushing -> Leaving -> Joining -> Off
```

| Stage | What happens | Limit |
|---|---|---|
| Start checks | Setting on, no airplane mode, random pool ready, tools and firmware present. Otherwise `FAILED` and nothing has changed | |
| Settling | Supplicant stopped, wait, link down, wait. The waits are those of the experiments | 2 s + 2 s |
| Starting | Private files, firmware name, link up, **firmware mode read from the kernel log: not 2, or not there, means failure**, hostapd; wait for `AP-ENABLED` in its log | 30 s |
| Serving | Address, DHCP, the page; state with name, password, address | **10 min** |
| Flushing | A form was accepted: its answer goes out. The password leaves the state at once | 1.5 s |
| Leaving | `SIGTERM` to the daemons, wait, `SIGKILL` to what is left, address flushed, link down, firmware name restored, files deleted, wait | 2 s + 2 s |
| Joining | `wpa_supplicant` and `udhcpc` on `wifi.conf.try`; a lease and a stored file mean success | 60 s |

The end is always one of these:
- **Joined:** `wifi.conf.try` is renamed to `wifi.conf` and `wifi_enabled` turns on.
- **Failed:** the tried file is deleted and the previous `wifi.conf` was never touched, so a typo cannot lose a working network. After 2 s for the supplicant to go, the radio goes back to what the settings say. A supplicant started before the old one has gone cannot get the control socket: it exits, and glassd tries again every 2 s, up to 5 times.
- If the network works and the file cannot be stored, that is a failure too (`files`): "joined" says it is kept.
- A cancel, the setting going off, or airplane mode work at any stage. In the 4 s before a join they stop the join. Airplane mode takes up to about 4 s to reach the Wi-Fi radio, for a clean stop.

While a session runs:
- the settings do not drive Wi-Fi (`Radio::hold_wifi`);
- the system does not suspend (`PowerPolicy::set_held`) and the off-head power-off waits. The behaviour of AP mode in suspend is unknown and a session is short;
- the companion's TCP listener and TCP connections are closed (section 4).

**If the daemon goes away.** `SIGTERM` and `SIGINT` end the session, then the daemon. After a `SIGKILL` or a crash nothing tells the daemons, so they are bounded by themselves: `timeout -s TERM {limit}`, a limit of about 660 s, a whole session plus a margin. So the access point can outlive the daemon by at most that, with its password. A restart ends it sooner: the next start finds the session's files (`cleanup_stale`) and ends it, with the same waits, before the settings drive the radio. That includes a crash while joining, which leaves a supplicant with the typed password: the tried file gives it away, and the pid files say which. A timed-out `hostapd` with no daemon leaves the firmware up in AP mode with nothing on it until the next start.

## 4. Security

**The access point password.** 12 characters from a 31-character alphabet without `0 o 1 l i`, by rejection sampling: about 59 bits. New for each session, never fixed, shown only through `WifiSetupState`. WPA2-PSK can be attacked offline from a captured handshake, and that would expose the typed password if the traffic was recorded too, so it has to resist for good and not just for ten minutes. At an assumed 10^6 PBKDF2 guesses a second (not measured) 59 bits takes about 18,000 years. It stays typable on a phone. The SSID is `OMG-Glass-xxxx`, random per session, not from the MAC.

**Randomness.** On 3.4 the `getrandom` crate would block the whole daemon on an unseeded pool, and the password would be weak on a thin one. A session needs `Companion::rng_ready`, the same gate as the identity. Otherwise `not-ready`. So `--companion off` means no session. Limit: on a later boot with no seed file (after a power cut between using one seed and saving the next) that gate is only "`/dev/random` readable". The link mixes its identity's secret in for that case; this password does not.

**No logging.**
- The access point password is in a 0600 file in a 0700 directory in `/var/run`, removed at the end, and never on a command line: `Radio::step` logs arguments on failure and `/proc` shows them.
- hostapd runs at its default level, never `-d`/`-K`.
- The typed password goes only into the 0600 config. `Debug` on `Network` and `Credentials` is redacted, and the portal drops its input buffer on answering.
- An integration test runs a whole session with `--verbose` and looks for the passwords and the token in the log.
- Not done: wiping memory. The password has to live in a file anyway.

**The page.**
- `SO_BINDTODEVICE` to `wlan0` as well as the address: Linux would otherwise accept a packet for `192.168.77.1` from `usb0` too. The listener exists only while Serving.
- Other pages in the phone's browser are the threat: they can post here but not read. So the form carries a per-session token, the `Host` must be this address (DNS rebinding), and a foreign `Origin` is refused.
- No script, nothing external. Strict CSP, `X-Frame-Options`, `no-store`, `nosniff`. Nothing typed is written back into a page.
- Only `GET /` and `POST /join`. At most 4 KiB per request, 4 connections, 10 s each. One accepted network ends the session.
- Refused: folded headers, repeated `Host` or `Content-Length`, `Transfer-Encoding`, bad escapes, non-UTF-8, unknown or repeated fields, control characters.
- The name and the password then go through `wpa_supplicant_conf`, the same validation as the QR path: the name in hex, the passphrase quoted, the case it cannot spell refused.

**Who can run a session.** This is more than the socket's usual trust level: whoever can start a session and read its password can post a form and replace the Glass's network, with nothing confirmed on the Glass, and traffic can then be sent through a network of their choosing. So apps must not reach it. glassd takes the peer's uid at connect (`SO_PEERCRED`); only its own uid and those given with `--privileged-uid` may send `WifiSetupRequest`, get `TOPIC_WIFI_SETUP`, or change `wifi_setup_ap_enabled`. Anyone else gets `ERROR_CODE_DENIED`, no topic and no change.

This is enforced only as far as uids differ. **Today's test images run everything as root, so everything there is trusted.** Requirements:
- SDK apps run under a uid that is neither glassd's nor named with `--privileged-uid`. The Applications section must guarantee this, or this gate is empty.
- The launcher sends `start: true` only from the owner's own gesture on that card, never by itself or on anyone's request, and shows the password on the prism only.
- The launcher does not pass the password, the state frames or the request on to any app, and does not log them.

There is no second confirmation of the network on the Glass. With the gate, the only way onto the access point is the password on the prism, so a form comes from someone who saw the prism or was told by the wearer. A later "Join {network}?" card would close the case of a wearer who was tricked, at the price of a step with a touchpad; see the owner decisions.

**Accepted risks.**
- Plain HTTP. The link is WPA2-encrypted and only someone holding the password shown on the Glass can be on it.

**The companion's TCP port.** An unbonded Glass can be bonded by anyone who reaches port 7700 (`link.rs`, `on_pair_request`). During a session the listener is closed (`Companion::set_tcp_closed`) and TCP connections dropped, from the start to after the end, so nothing on the access point can reach it. **That does not solve it on the owner's network afterwards**: on a home network any device can still bond with an unbonded Glass, and this very feature puts a Glass with no companion on a home network, which makes that case the usual one. Branch `glassd-ble-hardening` adds a pairing window, but its `may_pair` is `window_open || transport != Transport::Ble`, so TCP and RFCOMM stay open. It needs to become `window_open` for every transport, with a way to open the window before the launcher has its card (today its doc says "pair once over TCP").

## 5. Interface

- Setting `wifi_setup_ap_enabled`, default false, stored in `settings`.
- Capability `wifi-setup-ap`: radio control and a state directory exist.
- `ClientFrame.wifi_setup = WifiSetupRequest{start}`.
- `TOPIC_WIFI_SETUP`: `WifiSetupState{phase, ssid, password, url, seconds_left, clients, network, error, snapshot}`. See `proto/glass/services/v1/services.proto` for the phases and error codes.
- `seconds_left` is from when the frame was made, so a snapshot's is as old as the last change.
- `clients` comes from hostapd's log: `iw station dump` lists nothing on this driver (`sta=` empty in `kmsg_ap4`, with a client on).

Image needs: `hostapd` with libnl3 and libssl/libcrypto, `busybox-extras` (Alpine's busybox has no `udhcpd`), a `timeout` applet (in busybox 1.37 on the image; a coreutils one would forward `SIGTERM` but not `SIGKILL`), and the firmware at `/system/vendor/firmware/fw_bcmdhd.bin`. The firmware is not ours to redistribute and this code only links to it.

## 6. Launcher, not built

- It connects under a uid allowed by section 4, and follows the requirements there.
- Entry: in Wi-Fi settings, only with the capability and the setting on. The setting itself is a toggle there.
- On tap, and only on a tap: `WifiSetupRequest{start:true}`, subscribe to `TOPIC_WIFI_SETUP`, ask `ScreenRequest` keep-on. Keep-on is refused off the head, so the owner must wear the Glass to read the password.
- `STARTING`: "Starting", about 15 s.
- `SERVING`: three lines, large: name, password, `http://192.168.77.1/`, plus the time left and whether a device has joined. The password may be spaced in groups, but must say it has no spaces.
- `JOINING`: "Connecting to {network}". The password is off the prism on this frame, and must not be kept.
- `JOINED`, `FAILED`: result. For `join`: "Could not connect. Is it a 2.4 GHz network? Check the password.", with a retry, which opens a new session with a new password.
- Swipe down: `WifiSetupRequest{start:false}`.
- Strings in IT/EN/FR/ES. The page takes the language from `locale`.

## 7. Open questions

### 7.1 Phones with mobile data (needs a test, may need a decision)

The phone gets no internet from the access point. With mobile data on, Android may keep its default network on mobile, and a browser's connection to `192.168.77.1` may then not reach the Glass, or Android may offer to leave the Wi-Fi. iOS behaves differently again. No phone with a SIM has been tried. If this fails, the usual fix is a small DNS responder so that the phones' connectivity checks open a sign-in window: more code on an open port. Not built until a test shows it is needed.

### 7.2 Other

- No list of nearby networks: the name is typed, and one radio cannot scan while it is an access point. A scan before the session could fill a list; not built.
- A hidden network will not join (no `scan_ssid`).
- `wifi.conf` holds one network: a new one replaces the old.
- No "Join {network}?" card on the Glass after a form arrives (section 4): an owner decision.
- WPA2-Personal and open networks only. WPA3 is not offered: the firmware dates from 2014.

## 8. Test plan on the Glass

Prerequisites: an image with the tools in section 5, glassd from this branch started with `--radio on --verbose`, and a state directory. A session goes through the `wifi-setup` example of `glass-api`, built for the Glass, whose output must not go to `/dev/kmsg`: it holds the password. Read it through the RNDIS link (which came up unable to receive in 5 of 7 boots; toggling the gadget again fixes it), or put it on the prism with whatever the test image has. A network that is **not** the owner's home network for the first runs: a second phone's 2.4 GHz hotspot with a throwaway password, or the owner types it himself.

After each boot, keep `/proc/last_kmsg` before any other boot.

| # | Do | Pass if | Establishes |
|---|---|---|---|
| 1 | `wifi-setup ask` with the setting off | `FAILED error=disabled`, no change in `ip link` | Off by default |
| 2 | `wifi-setup start` right after a first boot with no seed | `not-ready`, daemon responsive; works later | The gate |
| 3 | `wifi-setup start` | kmsg `Firmware up: op_mode=0x0002`; `SERVING` in under 20 s; `iw dev wlan0 info` type AP | The glassd sequence, with `rfkill` and no `country_code` |
| 4 | Join from the Redmi with the shown password, 5 attempts, a reboot between | Joined 5/5. If not, compare with `ap3`, which failed once | The 1-in-3 failure |
| 5 | Browser on the Redmi, `http://192.168.77.1/`: **fill the form with a throwaway name and send it by hand**, in Chrome (tested only with Firefox 159 on a PC) | Page in the Glass's language; the send is answered with the "leaving" page, not a 403 | Reachable, DHCP, a real browser's `Origin` |
| 6 | From the PC on `usb0`, if up: `curl --interface usb0 http://192.168.77.1/` with a route added | No answer | `SO_BINDTODEVICE` on 3.4 |
| 7 | From the phone or a PC on the AP: port-scan the AP address, all TCP (`nmap -p- 192.168.77.1`, or an `nc` loop), and UDP 67; ping the `usb0` address; on Glass `cat /proc/sys/net/ipv4/ip_forward` | TCP 80 only, **7700 closed**, and it opens again after the session; no route to `usb0`; forward 0 | What the AP exposes |
| 8 | Send a wrong-password form for the hotspot | Page says it is leaving; `FAILED error=join` in about 70 s; old `wifi.conf` unchanged; **kmsg `op_mode=0x0005`**; with `wifi_enabled` on and a good old network: **it rejoins it within 30 s, no `wpa_supplicant` control-socket failure unretried in the log, `wpa_cli status` COMPLETED** | Failure path, normal firmware back, the rejoin |
| 9 | New session, right password, hotspot on 2.4 GHz | `JOINED`; `wifi.conf` mode 0600; IP from the hotspot; kmsg `op_mode=0x0005`; `/var/run/glassd-ap` gone; `AP` gone from `iw dev` | The whole path |
| 10 | Reboot after 9 | Joins by itself | Stored network |
| 11 | Session, send nothing, wait 10 min | `FAILED error=timeout`, hostapd and udhcpd gone, phone loses the network, Wi-Fi back to the setting | The timeout |
| 12 | Session, cable out, screen off, off head, 10 min | No suspend in kmsg; phone still reaches the page; then suspends normally | The hold |
| 13 | Session, airplane mode on | AP gone in under 5 s, Wi-Fi blocked | Airplane |
| 14 | Session, `kill -9` glassd, restart. Again, during `JOINING` of a network that will not answer. Each with `wifi_enabled` on and a good old network | hostapd gone within about 2 s of the restart; `/var/run/glassd-ap` and `wifi.conf.try` gone; no supplicant with the tried file; **it rejoins the old network**; a new session works | Stale cleanup, the rejoin |
| 15 | After 9 and 11: `grep` the last password and a typed one in kmsg, `hostapd.log` before it is deleted (copy during a session) and the glassd log | Not found | No logging, for real |
| 16 | A phone with a SIM and mobile data on, and an iPhone: join, then the page | Page reachable. If not: section 7.1 | **Owner decision** |
| 17 | `BLE` companion connected during a session | Link survives or drops, note which. Earlier logs show gaps of 3-4 s with Wi-Fi firmware up | Coexistence |
| 18 | After 9: `CORE OFF` count over 10 min cable out | Same as before a session | Nothing left awake by the AP |
| 19 | Session, `kill -9` glassd, do not restart, wait 11 min | hostapd and udhcpd gone by themselves at about 660 s; `ps` showed `$!`'s pid as hostapd itself | The bound, and busybox `timeout` |
| 20 | Session, `kill -TERM` glassd | glassd exits within about 3 s, AP and files gone, socket gone | The signal |
| 21 | Run a client as another uid (`su nobody`, socket group permitting): `wifi-setup start` | `ERROR_CODE_DENIED`, setting unchanged, no state frame | The gate on 3.4 |
