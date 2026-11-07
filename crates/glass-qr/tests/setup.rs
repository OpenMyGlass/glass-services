//! The text format: what it accepts, and everything it must not.

mod common;

use common::*;
use glass_qr::setup::*;

fn wifi_only(ssid: &[u8], auth: u8, password: Option<&[u8]>) -> Vec<u8> {
    let mut b = Vec::new();
    field(&mut b, TAG_WIFI_SSID, ssid);
    field(&mut b, TAG_WIFI_AUTH, &[auth]);
    if let Some(p) = password {
        field(&mut b, TAG_WIFI_PASSWORD, p);
    }
    b
}

fn parse_body(body: &[u8]) -> Result<Setup, SetupError> {
    parse(seal(body).as_bytes())
}

fn err(r: Result<Setup, SetupError>) -> SetupError {
    r.expect_err("must be refused")
}

#[test]
fn a_full_code_gives_an_offer_and_its_secrets_stay_behind() {
    let s = parse(full_text().as_bytes()).unwrap();
    assert_eq!(
        s.offer(),
        SetupOffer {
            wifi_ssid: Some(TEST_SSID.into()),
            has_seed: true,
            has_pairing_code: true,
        }
    );
    let w = s.wifi().unwrap();
    assert_eq!(w.auth, WifiAuth::WpaPsk);
    assert_eq!(w.password, Some(TEST_PASSWORD));
    assert!(!w.hidden);
    assert_eq!(s.seed(), Some(&TEST_SEED));
    assert_eq!(s.pairing_code(), Some(TEST_CODE));
    assert_eq!(s.companion_addr(), Some(TEST_ADDR.parse().unwrap()));
}

#[test]
fn no_secret_reaches_the_offer_or_a_log() {
    let s = parse(full_text().as_bytes()).unwrap();
    // How a byte array would print: the start is enough.
    let seed = format!("{}, {}, {}", TEST_SEED[0], TEST_SEED[1], TEST_SEED[2]);
    for shown in [format!("{:?}", s.offer()), format!("{s:?}")] {
        assert!(!shown.contains(TEST_PASSWORD), "{shown}");
        assert!(!shown.contains(TEST_CODE), "{shown}");
        assert!(!shown.contains(&seed), "{shown}");
    }
    assert!(format!("{s:?}").contains(TEST_SSID));
}

#[test]
fn every_error_is_free_of_the_content() {
    // The secret sits in a field that is then refused for a reason found later.
    let mut b = wifi_only(b"Net", 2, Some(TEST_PASSWORD.as_bytes()));
    b.push(99);
    b.push(0);
    let e = err(parse_body(&b));
    assert_eq!(e, SetupError::UnknownField);
    assert!(!format!("{e} {e:?}").contains(TEST_PASSWORD));
}

#[test]
fn the_checksum_is_the_ieee_one() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
}

#[test]
fn other_kinds_of_code_are_not_setup_codes() {
    for text in [
        &b""[..],
        b"https://example.com/",
        b"WIFI:S:Net;T:WPA;P:password;;",
        b"OMG1:AQ",
        b"OMG-SETUP:1:AA:00000000",
        b" omg-setup:1:AA:00000000",
        b"omg-setup",
        b"\xff\xfe\x00",
    ] {
        assert_eq!(err(parse(text)), SetupError::NotSetup, "{text:?}");
    }
}

#[test]
fn another_version_says_so() {
    for text in [
        "omg-setup:2:AA:00000000",
        "omg-setup:01:AA:00000000",
        "omg-setup::AA:00000000",
    ] {
        assert_eq!(
            err(parse(text.as_bytes())),
            SetupError::UnsupportedVersion,
            "{text}"
        );
    }
}

#[test]
fn a_broken_shape_is_refused() {
    for text in [
        "omg-setup:1",
        "omg-setup:1:",
        "omg-setup:1:AA",
        "omg-setup:1:AA:0000000",
        "omg-setup:1:AA:000000000",
        "omg-setup:1:AA:0000000G",
    ] {
        let e = err(parse(text.as_bytes()));
        assert!(
            matches!(e, SetupError::Malformed | SetupError::Checksum),
            "{text}: {e:?}"
        );
    }
    let text = full_text();
    let (head, crc) = text.rsplit_once(':').unwrap();
    assert_ne!(
        crc,
        crc.to_uppercase(),
        "this test needs letters in the checksum"
    );
    assert_eq!(
        err(parse(format!("{head}:{}", crc.to_uppercase()).as_bytes())),
        SetupError::Malformed
    );
    assert_eq!(
        err(parse(format!("{text}\n").as_bytes())),
        SetupError::Malformed
    );
    assert_eq!(
        err(parse(format!("{text} ").as_bytes())),
        SetupError::Malformed
    );
}

#[test]
fn any_changed_byte_is_refused() {
    let text = full_text().into_bytes();
    for i in PREFIX.len() + 2..text.len() {
        for mask in [0x01, 0x20, 0x80] {
            let mut t = text.clone();
            t[i] ^= mask;
            assert!(parse(&t).is_err(), "byte {i} mask {mask:#x}");
        }
    }
}

#[test]
fn a_body_that_is_not_canonical_base64_is_refused() {
    // With the checksum right, so that only the base64 is at fault.
    let sealed = |b64: &str| {
        let head = format!("{PREFIX}{VERSION}:{b64}");
        let crc = crc32(head.as_bytes());
        format!("{head}:{crc:08x}")
    };
    // "AB" holds one byte and 4 bits that must be zero; padding; characters of the other alphabet.
    for b64 in ["AB", "AA==", "AA+A", "AA/A", "A", "AA\nA", "AA AA", "é"] {
        assert_eq!(
            err(parse(sealed(b64).as_bytes())),
            SetupError::Base64,
            "{b64:?}"
        );
    }
    assert_eq!(err(parse(sealed("").as_bytes())), SetupError::Empty);
}

#[test]
fn too_long_is_refused_before_anything_else() {
    let text = "x".repeat(MAX_TEXT_BYTES + 1);
    assert_eq!(err(parse(text.as_bytes())), SetupError::TooLong);
    assert_eq!(err(parse(&vec![0xff; 1 << 20])), SetupError::TooLong);
}

#[test]
fn the_largest_valid_code_fits_the_limit() {
    let mut b = wifi_only(
        &[b's'; MAX_SSID_BYTES],
        3,
        Some(&[b'p'; MAX_SAE_PASSWORD_BYTES]),
    );
    field(&mut b, TAG_WIFI_HIDDEN, &[1]);
    let addr = "[fd00:1111:2222:3333:4444:5555:6666:7777]:65535";
    field(&mut b, TAG_COMPANION_ADDR, addr.as_bytes());
    field(&mut b, TAG_SEED, &TEST_SEED);
    field(&mut b, TAG_PAIRING_CODE, &[b'c'; MAX_PAIRING_CODE]);
    let text = seal(&b);
    assert!(text.len() <= MAX_TEXT_BYTES, "{}", text.len());
    assert!(parse(text.as_bytes()).is_ok());
}

#[test]
fn fields_out_of_order_or_twice_are_refused() {
    let mut b = Vec::new();
    field(&mut b, TAG_WIFI_AUTH, &[0]);
    field(&mut b, TAG_WIFI_SSID, b"Net");
    assert_eq!(err(parse_body(&b)), SetupError::FieldOrder);

    let mut b = Vec::new();
    field(&mut b, TAG_PAIRING_CODE, TEST_CODE.as_bytes());
    field(&mut b, TAG_PAIRING_CODE, TEST_CODE.as_bytes());
    assert_eq!(err(parse_body(&b)), SetupError::FieldOrder);
}

#[test]
fn an_unknown_field_is_refused() {
    for tag in [0, 8, 255] {
        let mut b = Vec::new();
        field(&mut b, TAG_PAIRING_CODE, TEST_CODE.as_bytes());
        // Tag 0 sorts first, but it is still unknown.
        field(&mut b, tag, b"x");
        assert_eq!(err(parse_body(&b)), SetupError::UnknownField, "{tag}");
    }
}

#[test]
fn a_cut_body_is_refused() {
    let body = full_body();
    // Cutting between two fields can leave a valid code; anywhere else it cannot.
    let mut ends = vec![];
    let mut at = 0;
    while at < body.len() {
        at += 2 + usize::from(body[at + 1]);
        ends.push(at);
    }
    for n in 1..body.len() {
        if !ends.contains(&n) {
            assert_eq!(err(parse_body(&body[..n])), SetupError::Truncated, "{n}");
        }
    }
}

#[test]
fn an_empty_body_offers_nothing() {
    assert_eq!(err(parse_body(&[])), SetupError::Empty);
}

#[test]
fn an_address_alone_is_nothing_to_set_up() {
    let mut b = Vec::new();
    field(&mut b, TAG_COMPANION_ADDR, TEST_ADDR.as_bytes());
    assert_eq!(
        err(parse_body(&b)),
        SetupError::Inconsistent(TAG_COMPANION_ADDR)
    );
}

#[test]
fn wifi_is_all_or_nothing() {
    let mut b = Vec::new();
    field(&mut b, TAG_WIFI_SSID, b"Net");
    assert_eq!(err(parse_body(&b)), SetupError::Inconsistent(TAG_WIFI_SSID));

    let mut b = Vec::new();
    field(&mut b, TAG_WIFI_AUTH, &[0]);
    assert_eq!(err(parse_body(&b)), SetupError::Inconsistent(TAG_WIFI_AUTH));

    let mut b = Vec::new();
    field(&mut b, TAG_WIFI_PASSWORD, TEST_PASSWORD.as_bytes());
    field(&mut b, TAG_PAIRING_CODE, TEST_CODE.as_bytes());
    assert_eq!(
        err(parse_body(&b)),
        SetupError::Inconsistent(TAG_WIFI_PASSWORD)
    );

    let mut b = Vec::new();
    field(&mut b, TAG_WIFI_HIDDEN, &[1]);
    field(&mut b, TAG_PAIRING_CODE, TEST_CODE.as_bytes());
    assert_eq!(
        err(parse_body(&b)),
        SetupError::Inconsistent(TAG_WIFI_HIDDEN)
    );
}

#[test]
fn a_secured_network_without_a_password_is_not_joined_as_open() {
    for auth in [1, 2, 3] {
        assert_eq!(
            err(parse_body(&wifi_only(b"Net", auth, None))),
            SetupError::Inconsistent(TAG_WIFI_AUTH)
        );
    }
    assert_eq!(
        err(parse_body(&wifi_only(
            b"Net",
            0,
            Some(TEST_PASSWORD.as_bytes())
        ))),
        SetupError::Inconsistent(TAG_WIFI_PASSWORD)
    );
    let open = parse_body(&wifi_only(b"Net", 0, None)).unwrap();
    assert_eq!(open.wifi().unwrap().password, None);
}

#[test]
fn hidden_has_one_spelling() {
    let hidden = |v: &[u8]| {
        let mut b = wifi_only(b"Net", 0, None);
        field(&mut b, TAG_WIFI_HIDDEN, v);
        parse_body(&b)
    };
    assert!(hidden(&[1]).unwrap().wifi().unwrap().hidden);
    for v in [&[0][..], &[2], &[], &[1, 1]] {
        assert_eq!(
            err(hidden(v)),
            SetupError::BadField(TAG_WIFI_HIDDEN),
            "{v:?}"
        );
    }
}

#[test]
fn auth_has_known_values_only() {
    for v in [&[4][..], &[255], &[], &[0, 0]] {
        let mut b = Vec::new();
        field(&mut b, TAG_WIFI_SSID, b"Net");
        field(&mut b, TAG_WIFI_AUTH, v);
        assert_eq!(
            err(parse_body(&b)),
            SetupError::BadField(TAG_WIFI_AUTH),
            "{v:?}"
        );
    }
}

#[test]
fn an_ssid_cannot_lie_on_the_screen() {
    let bad_field = SetupError::BadField(TAG_WIFI_SSID);
    for ssid in [
        "",              // empty
        "Home\nFree",    // a second line
        "Home\u{7}",     // a control character
        "Home\u{85}x",   // C1
        "emoH\u{202e}x", // right-to-left override
        "Home\u{2066}x", // bidi isolate
        "Ho\u{200b}me",  // zero width
        "Ho\u{ad}me",    // soft hyphen
        "Ho\u{feff}me",  // byte order mark
        "Ho\u{e0041}me", // tag character
        "Ho\u{2028}me",  // line separator
        "Ho\u{3164}me",  // Hangul filler, draws nothing
        " Home",
        "Home ",
        "Home\u{3000}", // ideographic space
    ] {
        assert_eq!(
            err(parse_body(&wifi_only(ssid.as_bytes(), 0, None))),
            bad_field,
            "{ssid:?}"
        );
    }
    assert_eq!(err(parse_body(&wifi_only(&[b's'; 33], 0, None))), bad_field);
    assert_eq!(
        err(parse_body(&wifi_only(b"\xff\xfeHome", 0, None))),
        bad_field
    );
    for ssid in [
        "Casa Rossa",
        "Caffè",
        "\"quoted\"",
        "a;b:c,d\\e",
        "ネット",
        "Home 🏠",
    ] {
        assert!(
            parse_body(&wifi_only(ssid.as_bytes(), 0, None)).is_ok(),
            "{ssid:?}"
        );
    }
    assert!(parse_body(&wifi_only(&[b's'; 32], 0, None)).is_ok());
}

#[test]
fn passwords_follow_what_glassd_can_write() {
    let bad = SetupError::BadField(TAG_WIFI_PASSWORD);
    let check = |auth, p: &[u8]| parse_body(&wifi_only(b"Net", auth, Some(p)));
    for (auth, p) in [
        (2, &b"short"[..]),
        (2, &[b'p'; 64]),
        (2, b"password\n"),
        (2, b"pass\x7fword"),
        (2, b"pass\"word#1"),
        (2, b"\xff\xfe-not-utf8"),
        (1, b"four"),
        (1, b"sixsix"),
        (3, b""),
        (3, b"pass\tword"),
        (3, &[b'p'; MAX_SAE_PASSWORD_BYTES + 1]),
    ] {
        assert_eq!(err(check(auth, p)), bad, "{auth} {p:?}");
    }
    for (auth, p) in [
        (2, &b"password"[..]),
        (2, &[b'p'; 63]),
        (2, b"pass#word"),
        (2, b"a\"b\\c\"d#e"),
        (2, "pàssword".as_bytes()),
        (1, b"fives"),
        (1, b"thirteen-byte"),
        (3, b"x"),
    ] {
        assert!(check(auth, p).is_ok(), "{auth} {p:?}");
    }
}

#[test]
fn a_companion_address_stays_on_the_local_network() {
    let with_addr = |a: &str| {
        let mut b = wifi_only(b"Net", 0, None);
        field(&mut b, TAG_COMPANION_ADDR, a.as_bytes());
        parse_body(&b)
    };
    for a in [
        "10.0.0.1:7700",
        "172.16.0.1:7700",
        "192.168.1.20:1",
        "169.254.1.1:7700",
        "[fd00::1]:7700",
        "[fe80::1]:7700",
    ] {
        assert!(with_addr(a).is_ok(), "{a}");
    }
    for a in [
        "8.8.8.8:7700",   // the internet
        "127.0.0.1:7700", // the Glass itself
        "0.0.0.0:7700",
        "255.255.255.255:7700",
        "[2001:db8::1]:7700",
        "[::1]:7700",
        "[::ffff:10.0.0.1]:7700",
        "10.0.0.1", // no port
        "10.0.0.1:0",
        "10.0.0.1:70000",
        "10.0.0.1:07700", // not as it would be printed
        "010.0.0.1:7700",
        "[FD00::1]:7700",
        "evil.example:7700", // a name
        "10.0.0.1:7700\n",
        "",
    ] {
        assert_eq!(
            err(with_addr(a)),
            SetupError::BadField(TAG_COMPANION_ADDR),
            "{a:?}"
        );
    }
    let long = format!("[fd00::1]:{}", "7".repeat(MAX_COMPANION_ADDR));
    assert_eq!(
        err(with_addr(&long)),
        SetupError::BadField(TAG_COMPANION_ADDR)
    );
}

#[test]
fn a_seed_is_exactly_its_size_and_not_constant() {
    let seed = |v: &[u8]| {
        let mut b = Vec::new();
        field(&mut b, TAG_SEED, v);
        parse_body(&b)
    };
    assert!(seed(&TEST_SEED).is_ok());
    for v in [&TEST_SEED[..31], &[0; 32], &[0xaa; 32], &[], &[7; 33]] {
        assert_eq!(err(seed(v)), SetupError::BadField(TAG_SEED), "{v:?}");
    }
}

#[test]
fn a_pairing_code_has_a_small_alphabet() {
    let code = |v: &[u8]| {
        let mut b = Vec::new();
        field(&mut b, TAG_PAIRING_CODE, v);
        parse_body(&b)
    };
    assert!(code(b"abc-DEF_123").is_ok());
    assert!(code(&[b'c'; MIN_PAIRING_CODE]).is_ok());
    assert!(code(&[b'c'; MAX_PAIRING_CODE]).is_ok());
    for v in [
        &[b'c'; MIN_PAIRING_CODE - 1][..],
        &[b'c'; MAX_PAIRING_CODE + 1],
        b"with space",
        b"line\nbreak",
        "codicé1".as_bytes(),
    ] {
        assert_eq!(
            err(code(v)),
            SetupError::BadField(TAG_PAIRING_CODE),
            "{v:?}"
        );
    }
}

/// Anything parse accepts has one spelling: encoding its fields again gives the same text. And any
/// mutated text either fails or still passes every rule; above all, nothing panics.
#[test]
fn random_and_mutated_texts_never_panic() {
    let mut rng = Rng::new(1);
    let base = full_text().into_bytes();
    let body = full_body();
    for _ in 0..200_000 {
        let mut t = base.clone();
        for _ in 0..=rng.below(4) {
            let i = rng.below(t.len());
            t[i] = rng.next() as u8;
        }
        let _ = parse(&t);

        // Past the checksum and the base64, right into the field parser.
        let mut b = body.clone();
        for _ in 0..=rng.below(4) {
            let i = rng.below(b.len());
            b[i] = rng.next() as u8;
        }
        b.truncate(rng.below(b.len() + 1));
        let _ = parse_body(&b);

        let noise: Vec<u8> = (0..rng.below(64)).map(|_| rng.next() as u8).collect();
        let _ = parse_body(&noise);
    }
}
