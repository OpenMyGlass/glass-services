# The BLE companion link on the Glass

BLE is the everyday link to the phone. Wi-Fi carries photos and video, or takes over when Bluetooth fails. The design and its measurements are in [`docs/plans/2026-11-05-ble-companion-link.md`](https://github.com/OpenMyGlass/OpenMyGlass/blob/main/docs/plans/2026-11-05-ble-companion-link.md) in OpenMyGlass, and the encrypted session that runs on top in its `docs/protocol/link-encryption.md`.

Status: tested on the host against a fake controller, down to the real process on a pseudo terminal. The radio path comes from a prototype that worked on the Glass, but **this code has not run on a Glass yet**, and deep sleep with the chip on has never been measured.

## Why a separate process

`omg-bled` owns the UART and the chip. For each phone session it connects to a Unix socket, and glassd treats what it accepts like any other companion connection (`Transport::Ble`).

- **The firmware patch takes about 6 s at 115200 baud.** That is either a blocking wait or a long state machine inside glassd's single poll loop. In its own process it is 30 lines that block.
- **It parses what anyone in radio range sends.** A bug here must not take the launcher's touch input down, and glassd only ever sees a byte stream.
- **glassd stays as it is.** One more listener, one more descriptor for the engine's output, no async runtime, and `Stream`, `Link` and note sync have nothing new to learn.
- The cost is one more process and one more hop per wake. The process sleeps in `poll` and adds no timer: a test counts its context switches while idle and while connected, and expects none.

## The stream

Each write (phone to Glass) and each notification (Glass to phone) is the next run of bytes of the `omg-core` stream, with no header. `FrameDecoder` already takes bytes in any splits.

| Item | UUID | Properties |
|---|---|---|
| Service | `6f6d672d-626c-652d-6c69-6e6b2d763100` | |
| `rx`, phone to Glass | `...763101` | Write, Write Without Response |
| `tx`, Glass to phone | `...763102` | Notify, with CCCD |

- **A session is one LE connection with `tx` subscribed.** Nothing crosses to the next session: the encrypted link would fail on it anyway. The phone does a new handshake, and notes resume from disk.
- **Glass to phone:** bytes leave glassd's socket only while the engine has room (1 KiB queued), and fragments go to the controller only against its credits. A stalled radio stalls the socket, and nothing grows. The kernel buffer on that socket is set small, because the default would be close to a minute of queue at BLE speed.
- **Phone to Glass:** the phone paces its writes (`omg_core::ble`). If glassd falls behind by 16 KiB the engine stops reading the UART, and RTS and the link layer hold the phone back.
- **Answers overtake the stream** at a PDU's boundary, so GATT requests do not wait behind notifications.

## Sizes: the rule

The side that pulls chooses the chunk size, so the rule is the profile the phone gives a BLE link (`LinkProfile::constrained()`, and `BLE_PROFILE` for what the Glass pulls):

- **1 KiB chunks, two in flight.** A status or notification frame waits about a second at most (test: at most 4 KiB ahead of it).
- **Text and attachments up to 256 KiB sync by themselves**, at about 90 s for the largest.
- **A bigger attachment stays on the Glass** with `Deferred{NeedsFasterLink}`, without holding up other notes, and syncs on the next link with no limit, Wi-Fi today.
- A frame of any size still crosses, slowly: a peer that asks for 48 KiB chunks gets them (tested). The stream has no frame limit of its own.

Nothing yet asks the Glass to bring Wi-Fi up for a waiting attachment: `WifiBringUpResult` is still not sent. Until then the owner connects over Wi-Fi by hand.

TTS audio has a path: `NoteTts*` chunk requests use `BLE_PROFILE` like any other pull. As 16-bit PCM it is ten times the link's speed, so a sentence takes as long to fetch as several to play. A codec is what would make it fit.

## Power, and the UART

Deep sleep depends on UART2 idling. What changes:

| Bluetooth setting | Before | Now, with `--companion-ble on` |
|---|---|---|
| Off | `rfkill block`, then one byte on the port, once per run | The same. Nothing opens the port |
| On | `rfkill unblock`, `hciconfig` | `rfkill unblock`, then the engine power-cycles the chip and **holds the port open**, with the chip in `Write Sleep Mode` so that BT_WAKE and HOST_WAKE let the UART idle between events |
| On to off | `rfkill block`, kick | Engine stops first and the port closes, then `rfkill block`, then the kick if it is still due |
| Engine dies | | `rfkill block` and the kick at once, then up to 3 tries at 5 s, 30 s, 120 s. A missing firmware gets no try. A chip with nobody at the port must not hold the deep sleep back |

- **The kick** (`kick_bt_uart`) is for a port that has never transmitted. An engine that came up has transmitted, so the kick is dropped then. It must also never land while the engine owns the port, because a stray byte would break its HCI stream, and it runs only after the engine has gone.
- **Whether CORE OFF still happens with the port open and the chip on is not known**, and that is why `--companion-ble` is off by default (experiment E5). The prototype's measured runs with the sleep mode on were with the cable in, so no deep sleep at all.
- **`omg-bled` asks for 285-300 ms, latency 5** after 5 s of quiet, and for 15-30 ms when more than 200 bytes wait. Whether a phone agrees is its choice, and a refusal is not repeated on that link.
- **A wake** goes HOST_WAKE, UART, `omg-bled`, socket, glassd's `poll`, then `resume_from_suspend` as on a Wi-Fi wake. Not run yet (E6).
- **While the engine runs, the kernel's Bluetooth stack is not used.** No RFCOMM, A2DP or HFP alongside it, as the owner decided.

## Limits, on purpose

- **Pairing stays as it is:** open, trust on first use, on every transport.
- One phone at a time. A central that does not subscribe goes after 30 s, and one that does not finish the handshake after 30 s, because a connected stranger stops the advertising. A stranger coming back again and again can still keep the owner out.
- The address is static and random, kept in `<state-dir>/ble-address`, so the Glass can be recognised by anyone in range.
- No Bluetooth bonding: SMP is refused, and nothing relies on it.

## Running it on a Glass

The image needs `glassd` and `omg-bled` side by side, for example in `/sbin`, and the patch at `/vendor/firmware/bcm4330.hcd`. The patch is a vendor file: it comes from the stock system (`model-a-kitkat-salvage/vendor-glass-1/proprietary/vendor/firmware/bcm4330.hcd`), and it goes on local test images only.

```sh
mkdir -p /run/glassd /var/lib/glassd
[ -e /dev/ttyO1 ] || { mm=$(cat /sys/class/tty/ttyO1/dev); mknod /dev/ttyO1 c ${mm%%:*} ${mm##*:}; }
export GLASSD_STATE_DIR=/var/lib/glassd
glassd -v --radio on --companion-ble on --display fb --socket /run/glassd/glassd.sock 2>&1 | ...
```

On a new state directory Bluetooth comes up by itself. An existing `settings` file keeps its `bluetooth_enabled`. Look in the log for `omg-bled: controller up`, `omg-bled: advertising`, `radio: BLE engine ready`, `companion: BLE up`, then `phone connected`, `companion: a phone over BLE` and `connected over ble`.

| Flag | Default | |
|---|---|---|
| `--companion-ble` | `off` | Needs `--radio on` and `--companion on` |
| `--ble-engine` | next to glassd | Without it on the image: as if off |
| `--ble-firmware` | `/vendor/firmware/bcm4330.hcd` | |
| `--ble-socket` | `/run/glassd/ble.sock` | Mode 0600 |
