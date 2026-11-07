//! The `omg-setup` text format and its validation.
//!
//! ```text
//! omg-setup:1:<base64url of the body, no padding>:<CRC-32 in 8 lowercase hex digits>
//! ```
//!
//! The CRC-32 (IEEE, the zlib one) covers everything before the last `:`. It only tells a setup
//! code from a misread or an unrelated one: anyone can compute it, so **it proves nothing about
//! who made the code**. The user's tap on the offer is what stops a poster or a passer-by's screen.
//!
//! The body is a list of fields, each `tag (1 byte) length (1 byte) value`, in strictly ascending
//! tag order, so a field cannot appear twice and one content has one encoding. An unknown tag is
//! refused, because it is what a newer format would add. See the `TAG_` constants for the fields.
//!
//! What is checked, beyond the shape:
//!
//! - **Wi-Fi** is all or nothing: SSID and auth together, a password if and only if the network
//!   is not open. A code cannot ask for a secured network and leave it to be joined as open.
//! - **SSID:** UTF-8 with no control character. It is joined as it is, but the offer shows it
//!   through [`display_ssid`], which writes as an escape every character that could look like
//!   another or like nothing. An SSID that is not UTF-8 cannot be offered.
//! - **Password:** the rules of `wpa_supplicant_conf` in glassd, which writes it. A test in glassd
//!   holds the two together.
//! - **Companion address:** a numeric socket address in a private or link-local range, because a
//!   code should not be able to send the Glass to a server on the internet.
//!
//! - **Seed:** mixed into the Glass's pool, never trusted: see [`MixOnlySeed`].
//!
//! The offer shows everything a code would change: the network and how it is secured, the
//! companion it would dial, and whether it pairs a new companion. A hostile code is one the user
//! could tell from the real one only by these.
//!
//! Errors say what was wrong and never hold any of the content, so they are safe to log.

use std::net::{IpAddr, SocketAddr};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use zeroize::Zeroizing;

/// What comes before the version.
pub const PREFIX: &str = "omg-setup:";
/// The only format version this code reads.
pub const VERSION: &str = "1";
/// The longest text accepted, in bytes. Every field at its largest fits, with room to spare.
pub const MAX_TEXT_BYTES: usize = 512;

/// The network's name, UTF-8, 1 to [`MAX_SSID_BYTES`] bytes. Requires [`TAG_WIFI_AUTH`].
pub const TAG_WIFI_SSID: u8 = 1;
/// One byte: 0 open, 1 WEP, 2 WPA-PSK, 3 WPA3-SAE.
pub const TAG_WIFI_AUTH: u8 = 2;
/// The password, UTF-8. Present exactly when the network is not open.
pub const TAG_WIFI_PASSWORD: u8 = 3;
/// One byte, always 1: the SSID is hidden. Absent means it is not.
pub const TAG_WIFI_HIDDEN: u8 = 4;
/// The companion's address as text, such as `192.168.1.20:7700`.
pub const TAG_COMPANION_ADDR: u8 = 5;
/// Bytes to mix into the Glass's pool, exactly [`SEED_BYTES`]. Never credited: see [`MixOnlySeed`].
pub const TAG_SEED: u8 = 6;
/// The pairing code, [`MIN_PAIRING_CODE`] to [`MAX_PAIRING_CODE`] of `A-Za-z0-9_-`.
pub const TAG_PAIRING_CODE: u8 = 7;

/// An SSID is at most this long (802.11).
pub const MAX_SSID_BYTES: usize = 32;
/// A WPA3 password is at most this long. The standard sets no limit; a QR needs one.
pub const MAX_SAE_PASSWORD_BYTES: usize = 128;
/// Size of a seed from a code.
pub const SEED_BYTES: usize = 32;
/// The shortest pairing code.
pub const MIN_PAIRING_CODE: usize = 6;
/// The longest pairing code.
pub const MAX_PAIRING_CODE: usize = 64;
/// The longest companion address: more than `[<IPv6>%<scope>]:<port>` can need.
pub const MAX_COMPANION_ADDR: usize = 64;

/// How a network authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WifiAuth {
    Open,
    Wep,
    WpaPsk,
    Wpa3Sae,
}

/// Why a text is not a setup offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupError {
    /// Longer than [`MAX_TEXT_BYTES`].
    TooLong,
    /// Some other kind of code: a link, a plain Wi-Fi QR, anything.
    NotSetup,
    /// A setup code of a version this code does not read.
    UnsupportedVersion,
    /// The text has the prefix and then not the shape of the format.
    Malformed,
    /// The checksum does not match.
    Checksum,
    /// The body is not canonical base64url.
    Base64,
    /// A field runs past the end of the body.
    Truncated,
    /// A field this version does not know.
    UnknownField,
    /// Fields out of order, or one given twice.
    FieldOrder,
    /// A field's content breaks its rules: this tag.
    BadField(u8),
    /// A field that needs another one, or excludes one: this tag.
    Inconsistent(u8),
    /// No Wi-Fi, seed or pairing code: nothing to offer.
    Empty,
}

impl std::fmt::Display for SetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupError::TooLong => write!(f, "too long"),
            SetupError::NotSetup => write!(f, "not a setup code"),
            SetupError::UnsupportedVersion => write!(f, "unsupported setup version"),
            SetupError::Malformed => write!(f, "malformed setup code"),
            SetupError::Checksum => write!(f, "bad checksum"),
            SetupError::Base64 => write!(f, "bad base64"),
            SetupError::Truncated => write!(f, "truncated field"),
            SetupError::UnknownField => write!(f, "unknown field"),
            SetupError::FieldOrder => write!(f, "fields out of order"),
            SetupError::BadField(t) => write!(f, "bad content in field {t}"),
            SetupError::Inconsistent(t) => write!(f, "field {t} does not fit the others"),
            SetupError::Empty => write!(f, "nothing to set up"),
        }
    }
}

impl std::error::Error for SetupError {}

/// What the launcher may see: everything the code would change, and no secret. The launcher shows
/// all of it before the user's tap, so that a hostile code differs from the real one on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupOffer {
    /// The network it would join, as [`display_ssid`] writes it. Show it as it is.
    pub wifi_ssid: Option<String>,
    /// How that network is secured. [`WifiAuth::Open`] is worth saying.
    pub wifi_auth: Option<WifiAuth>,
    /// The companion it would dial.
    pub companion_addr: Option<SocketAddr>,
    /// The code pairs a new companion: it carries a pairing code. Say so plainly, because it
    /// changes who the Glass trusts.
    pub pairs_new_companion: bool,
    pub has_seed: bool,
}

/// A network from a setup code, to apply after the user has confirmed.
pub struct WifiNetwork<'a> {
    pub ssid: &'a str,
    pub auth: WifiAuth,
    /// `Some` exactly when `auth` is not [`WifiAuth::Open`].
    pub password: Option<&'a str>,
    pub hidden: bool,
}

impl std::fmt::Debug for WifiNetwork<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WifiNetwork")
            .field("ssid", &self.ssid)
            .field("auth", &self.auth)
            .field("password", &self.password.map(|_| "<secret>"))
            .field("hidden", &self.hidden)
            .finish()
    }
}

/// A validated setup code, secrets included. It stays with the side that applies it: pass
/// [`Setup::offer`] to anything else. Secrets are wiped when it is dropped, and it cannot be
/// cloned or printed with them.
pub struct Setup {
    wifi: Option<Wifi>,
    companion_addr: Option<SocketAddr>,
    seed: Option<MixOnlySeed>,
    pairing_code: Option<Zeroizing<String>>,
}

struct Wifi {
    ssid: String,
    auth: WifiAuth,
    password: Option<Zeroizing<String>>,
    hidden: bool,
}

impl std::fmt::Debug for Setup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Setup").field(&self.offer()).finish()
    }
}

/// Bytes from a setup code for the Glass's random pool. **They are not entropy:** they travel in the
/// open on a screen, and a hostile code chooses them. Write them to `/dev/urandom` without crediting
/// them, which can only add to the pool. They never stand in for glassd's wait for
/// `MIN_BITS_FOR_IDENTITY`, nor for its installer seed file, so a first identity never rests on
/// them. This type offers its bytes for that one use and nothing else.
pub struct MixOnlySeed(Zeroizing<Box<[u8]>>);

impl MixOnlySeed {
    /// The bytes to write to `/dev/urandom`, uncredited.
    pub fn bytes_to_mix_uncredited(&self) -> &[u8] {
        &self.0
    }
}

impl Setup {
    /// The view for the launcher.
    pub fn offer(&self) -> SetupOffer {
        SetupOffer {
            wifi_ssid: self.wifi.as_ref().map(|w| display_ssid(&w.ssid)),
            wifi_auth: self.wifi.as_ref().map(|w| w.auth),
            companion_addr: self.companion_addr,
            pairs_new_companion: self.pairing_code.is_some(),
            has_seed: self.seed.is_some(),
        }
    }

    pub fn wifi(&self) -> Option<WifiNetwork<'_>> {
        self.wifi.as_ref().map(|w| WifiNetwork {
            ssid: &w.ssid,
            auth: w.auth,
            password: w.password.as_ref().map(|p| p.as_str()),
            hidden: w.hidden,
        })
    }

    pub fn companion_addr(&self) -> Option<SocketAddr> {
        self.companion_addr
    }

    /// See [`MixOnlySeed`]: not entropy.
    pub fn seed(&self) -> Option<&MixOnlySeed> {
        self.seed.as_ref()
    }

    pub fn pairing_code(&self) -> Option<&str> {
        self.pairing_code.as_ref().map(|c| c.as_str())
    }

    /// The one text that gives this setup: [`parse`] of it gives the same setup back.
    pub fn to_text(&self) -> Zeroizing<String> {
        let mut body = Zeroizing::new(Vec::with_capacity(MAX_TEXT_BYTES));
        let mut field = |tag: u8, value: &[u8]| {
            body.push(tag);
            body.push(value.len() as u8);
            body.extend_from_slice(value);
        };
        if let Some(w) = &self.wifi {
            field(TAG_WIFI_SSID, w.ssid.as_bytes());
            let auth = match w.auth {
                WifiAuth::Open => 0,
                WifiAuth::Wep => 1,
                WifiAuth::WpaPsk => 2,
                WifiAuth::Wpa3Sae => 3,
            };
            field(TAG_WIFI_AUTH, &[auth]);
            if let Some(p) = &w.password {
                field(TAG_WIFI_PASSWORD, p.as_bytes());
            }
            if w.hidden {
                field(TAG_WIFI_HIDDEN, &[1]);
            }
        }
        if let Some(a) = self.companion_addr {
            field(TAG_COMPANION_ADDR, a.to_string().as_bytes());
        }
        if let Some(s) = &self.seed {
            field(TAG_SEED, s.bytes_to_mix_uncredited());
        }
        if let Some(c) = &self.pairing_code {
            field(TAG_PAIRING_CODE, c.as_bytes());
        }
        Zeroizing::new(seal(&body))
    }
}

/// An SSID as the launcher shows it. Kept as they are: printable ASCII, and the precomposed letters
/// and signs of Latin-1 and Latin Extended-A that the Glass font draws, less those that pass for
/// others (no-break space, soft hyphen, long s, dotless i). Anything else, a backslash, and a space
/// at either end are written `\u{hex}`, so that what shows is what would be joined: no combining
/// marks, no blank or invisible characters, no other forms of the same letters.
pub fn display_ssid(ssid: &str) -> String {
    let last = ssid.chars().count().saturating_sub(1);
    let mut out = String::with_capacity(ssid.len());
    for (i, c) in ssid.chars().enumerate() {
        let shown = match c {
            ' ' => i != 0 && i != last,
            '\\' => false,
            '!'..='~' => true,
            '\u{a0}' | '\u{ad}' | '\u{17f}' | '\u{131}' => false,
            '\u{a1}'..='\u{17f}' => true,
            _ => false,
        };
        if shown {
            out.push(c);
        } else {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        }
    }
    out
}

/// CRC-32 (IEEE). Small and slow on purpose: it runs once, over at most [`MAX_TEXT_BYTES`].
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// The text for a body, which this does not check: the other half of [`parse`], for the side that
/// generates codes and for tests.
pub fn seal(body: &[u8]) -> String {
    let head = format!("{PREFIX}{VERSION}:{}", URL_SAFE_NO_PAD.encode(body));
    let crc = crc32(head.as_bytes());
    format!("{head}:{crc:08x}")
}

/// Validates a decoded QR text.
pub fn parse(text: &[u8]) -> Result<Setup, SetupError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(SetupError::TooLong);
    }
    let rest = text
        .strip_prefix(PREFIX.as_bytes())
        .ok_or(SetupError::NotSetup)?;
    // The version comes first and alone: a newer code's other parts may differ in any way.
    let colon = rest
        .iter()
        .position(|&b| b == b':')
        .ok_or(SetupError::Malformed)?;
    let (version, rest) = (&rest[..colon], &rest[colon + 1..]);
    if version != VERSION.as_bytes() {
        return Err(SetupError::UnsupportedVersion);
    }
    if rest.len() < 9 || rest[rest.len() - 9] != b':' {
        return Err(SetupError::Malformed);
    }
    let (b64, crc) = rest.split_at(rest.len() - 9);
    let want = parse_crc(&crc[1..]).ok_or(SetupError::Malformed)?;
    if crc32(&text[..text.len() - 9]) != want {
        return Err(SetupError::Checksum);
    }
    let body = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(b64)
            .map_err(|_| SetupError::Base64)?,
    );
    parse_body(&body)
}

/// Exactly 8 lowercase hex digits: `from_str_radix` would also take capitals and a `+`.
fn parse_crc(hex: &[u8]) -> Option<u32> {
    let mut v = 0u32;
    for &b in hex {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            _ => return None,
        };
        v = v << 4 | u32::from(d);
    }
    Some(v)
}

fn parse_body(mut body: &[u8]) -> Result<Setup, SetupError> {
    let mut ssid = None;
    let mut auth = None;
    let mut password = None;
    let mut hidden = false;
    let mut companion_addr = None;
    let mut seed = None;
    let mut pairing_code = None;
    let mut last = 0u8;
    while let Some((&[tag, len], rest)) = body.split_first_chunk::<2>() {
        let len = usize::from(len);
        if !(TAG_WIFI_SSID..=TAG_PAIRING_CODE).contains(&tag) {
            return Err(SetupError::UnknownField);
        }
        if tag <= last {
            return Err(SetupError::FieldOrder);
        }
        last = tag;
        if rest.len() < len {
            return Err(SetupError::Truncated);
        }
        let (value, rest) = rest.split_at(len);
        body = rest;
        let bad = SetupError::BadField(tag);
        match tag {
            TAG_WIFI_SSID => ssid = Some(parse_ssid(value).ok_or(bad)?),
            TAG_WIFI_AUTH => auth = Some(parse_auth(value).ok_or(bad)?),
            TAG_WIFI_PASSWORD => {
                password = Some(Zeroizing::new(
                    str::from_utf8(value).map_err(|_| bad)?.to_owned(),
                ));
            }
            TAG_WIFI_HIDDEN if value == [1] => hidden = true,
            TAG_COMPANION_ADDR => companion_addr = Some(parse_addr(value).ok_or(bad)?),
            TAG_SEED => seed = Some(parse_seed(value).ok_or(bad)?),
            TAG_PAIRING_CODE => pairing_code = Some(parse_pairing_code(value).ok_or(bad)?),
            _ => return Err(bad),
        }
    }
    if !body.is_empty() {
        return Err(SetupError::Truncated);
    }

    let wifi = match (ssid, auth) {
        (None, None) => {
            if password.is_some() {
                return Err(SetupError::Inconsistent(TAG_WIFI_PASSWORD));
            }
            if hidden {
                return Err(SetupError::Inconsistent(TAG_WIFI_HIDDEN));
            }
            None
        }
        (Some(_), None) => return Err(SetupError::Inconsistent(TAG_WIFI_SSID)),
        (None, Some(_)) => return Err(SetupError::Inconsistent(TAG_WIFI_AUTH)),
        (Some(ssid), Some(auth)) => {
            check_password(auth, password.as_ref().map(|p| p.as_str()))?;
            Some(Wifi {
                ssid,
                auth,
                password,
                hidden,
            })
        }
    };
    if wifi.is_none() && seed.is_none() && pairing_code.is_none() {
        return Err(if companion_addr.is_some() {
            SetupError::Inconsistent(TAG_COMPANION_ADDR)
        } else {
            SetupError::Empty
        });
    }
    Ok(Setup {
        wifi,
        companion_addr,
        seed,
        pairing_code,
    })
}

fn parse_ssid(value: &[u8]) -> Option<String> {
    if value.is_empty() || value.len() > MAX_SSID_BYTES {
        return None;
    }
    let s = str::from_utf8(value).ok()?;
    (!s.chars().any(char::is_control)).then(|| s.to_owned())
}

fn parse_auth(value: &[u8]) -> Option<WifiAuth> {
    match value {
        [0] => Some(WifiAuth::Open),
        [1] => Some(WifiAuth::Wep),
        [2] => Some(WifiAuth::WpaPsk),
        [3] => Some(WifiAuth::Wpa3Sae),
        _ => None,
    }
}

/// The same limits as `wpa_supplicant_conf` in glassd, for each kind of network.
fn check_password(auth: WifiAuth, password: Option<&str>) -> Result<(), SetupError> {
    let bad = SetupError::BadField(TAG_WIFI_PASSWORD);
    let Some(p) = password else {
        return match auth {
            WifiAuth::Open => Ok(()),
            _ => Err(SetupError::Inconsistent(TAG_WIFI_AUTH)),
        };
    };
    if p.bytes().any(|b| b < 32 || b == 127) {
        return Err(bad);
    }
    let fits = match auth {
        WifiAuth::Open => return Err(SetupError::Inconsistent(TAG_WIFI_PASSWORD)),
        WifiAuth::Wep => [5, 13, 16].contains(&p.len()),
        WifiAuth::Wpa3Sae => (1..=MAX_SAE_PASSWORD_BYTES).contains(&p.len()),
        WifiAuth::WpaPsk => (8..=63).contains(&p.len()) && !hash_after_odd_quote(p),
    };
    if fits { Ok(()) } else { Err(bad) }
}

/// A `#` after an odd number of `"` cannot be written as a quoted passphrase: the supplicant would
/// read it as a comment. See `quoted_passphrase` in glassd.
fn hash_after_odd_quote(p: &str) -> bool {
    let mut odd = false;
    for b in p.bytes() {
        match b {
            b'"' => odd = !odd,
            b'#' if odd => return true,
            _ => {}
        }
    }
    false
}

/// A number, in the form it would be printed in, in a range that stays on the local network.
fn parse_addr(value: &[u8]) -> Option<SocketAddr> {
    if value.len() > MAX_COMPANION_ADDR {
        return None;
    }
    let text = str::from_utf8(value).ok()?;
    let addr: SocketAddr = text.parse().ok()?;
    let local = match addr.ip() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            first & 0xfe00 == 0xfc00 || first & 0xffc0 == 0xfe80
        }
    };
    (local && addr.port() != 0 && addr.to_string() == text).then_some(addr)
}

/// Exactly its size, copied straight to the heap so that it never moves. All one byte is refused
/// as a sign of a broken generator; that is no test of randomness, nor meant as one.
fn parse_seed(value: &[u8]) -> Option<MixOnlySeed> {
    if value.len() != SEED_BYTES || value.iter().all(|&b| b == value[0]) {
        return None;
    }
    Some(MixOnlySeed(Zeroizing::new(value.into())))
}

fn parse_pairing_code(value: &[u8]) -> Option<Zeroizing<String>> {
    let fits = (MIN_PAIRING_CODE..=MAX_PAIRING_CODE).contains(&value.len())
        && value
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    if !fits {
        return None;
    }
    str::from_utf8(value)
        .ok()
        .map(|s| Zeroizing::new(s.to_owned()))
}
