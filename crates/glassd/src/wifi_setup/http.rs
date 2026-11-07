//! The setup page's HTTP, without sockets: one request's bytes in, one response's bytes out.
//!
//! This parser is small on purpose and refuses what it does not need. A request comes from a
//! browser on the setup network, and the other pages open in that browser are the threat: they can
//! send a form to this address but cannot read the page. So a form must carry the token from the
//! page, a `Host` other than this address (DNS rebinding) is refused, and so is a foreign `Origin`.
//! Nothing the visitor sent is ever written back into a page.

use crate::companion::bootstrap::{WifiAuth, wpa_supplicant_conf};

/// The largest request, head and body. A network name and a password, encoded three bytes per
/// byte, take well under this.
pub const MAX_REQUEST: usize = 4096;

/// A network the visitor asked for, with its finished `wpa_supplicant` config. It holds the
/// password, so its `Debug` shows neither.
#[derive(Clone, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    pub conf: String,
}

impl std::fmt::Debug for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Network(redacted)")
    }
}

/// The page's language: the Glass's own, from the `locale` setting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    En,
    It,
    Fr,
    Es,
}

/// What this session's requests are checked against.
#[derive(Debug, Clone)]
pub struct Site {
    /// The address the page is served on, as a `Host` header has it (`192.168.77.1`, or with a
    /// port when it is not 80).
    pub host: String,
    /// The per-session form token, hex.
    pub token: String,
    pub lang: Lang,
}

/// What a request's bytes come to so far.
#[derive(Debug, PartialEq, Eq)]
pub enum Handled {
    /// Not a whole request yet.
    NeedMore,
    /// Send this and close.
    Respond(Vec<u8>),
    /// Send this and close: the visitor gave a valid network.
    Accepted(Vec<u8>, Network),
}

/// Why a form was refused. It picks the message; it never holds what was typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Not a well-formed form with exactly the known fields.
    Malformed,
    /// The token is not this session's.
    Token,
    /// The network name is empty, over 32 bytes, or has control characters.
    Ssid,
    /// A password is needed and none came, or one came for an open network.
    PasswordChoice,
    /// The password is not one `wpa_supplicant` can take.
    Password,
}

struct Text {
    title: &'static str,
    intro: &'static str,
    ssid: &'static str,
    password: &'static str,
    open: &'static str,
    send: &'static str,
    joining: &'static str,
    bad_ssid: &'static str,
    bad_choice: &'static str,
    bad_password: &'static str,
    bad_form: &'static str,
}

fn text(lang: Lang) -> &'static Text {
    match lang {
        Lang::En => &Text {
            title: "Glass Wi-Fi setup",
            intro: "Glass can only use 2.4 GHz networks.",
            ssid: "Network name",
            password: "Password",
            open: "This network has no password",
            send: "Connect",
            joining: "Glass is leaving this network to connect. Look at the Glass for the result.",
            bad_ssid: "The network name must be 1 to 32 bytes, with no control characters.",
            bad_choice: "Type a password, or tick the box for a network without one.",
            bad_password: "The password must be 8 to 63 characters. A \" followed later by a # is not supported.",
            bad_form: "This page has expired. Reload it and try again.",
        },
        Lang::It => &Text {
            title: "Wi-Fi di Glass",
            intro: "Glass può usare solo reti a 2,4 GHz.",
            ssid: "Nome della rete",
            password: "Password",
            open: "Questa rete non ha password",
            send: "Connetti",
            joining: "Glass lascia questa rete per connettersi. Guarda il risultato su Glass.",
            bad_ssid: "Il nome della rete deve avere da 1 a 32 byte, senza caratteri di controllo.",
            bad_choice: "Scrivi una password, o spunta la casella per una rete senza password.",
            bad_password: "La password deve avere da 8 a 63 caratteri. Un \" seguito più avanti da un # non è supportato.",
            bad_form: "Questa pagina è scaduta. Ricaricala e riprova.",
        },
        Lang::Fr => &Text {
            title: "Wi-Fi de Glass",
            intro: "Glass ne peut utiliser que les réseaux 2,4 GHz.",
            ssid: "Nom du réseau",
            password: "Mot de passe",
            open: "Ce réseau n'a pas de mot de passe",
            send: "Connecter",
            joining: "Glass quitte ce réseau pour se connecter. Regardez le résultat sur Glass.",
            bad_ssid: "Le nom du réseau doit faire de 1 à 32 octets, sans caractères de contrôle.",
            bad_choice: "Saisissez un mot de passe, ou cochez la case pour un réseau sans mot de passe.",
            bad_password: "Le mot de passe doit faire de 8 à 63 caractères. Un \" suivi plus loin d'un # n'est pas pris en charge.",
            bad_form: "Cette page a expiré. Rechargez-la et réessayez.",
        },
        Lang::Es => &Text {
            title: "Wi-Fi de Glass",
            intro: "Glass solo puede usar redes de 2,4 GHz.",
            ssid: "Nombre de la red",
            password: "Contraseña",
            open: "Esta red no tiene contraseña",
            send: "Conectar",
            joining: "Glass deja esta red para conectarse. Mira el resultado en Glass.",
            bad_ssid: "El nombre de la red debe tener de 1 a 32 bytes, sin caracteres de control.",
            bad_choice: "Escribe una contraseña, o marca la casilla para una red sin contraseña.",
            bad_password: "La contraseña debe tener de 8 a 63 caracteres. Un \" seguido más adelante de un # no se admite.",
            bad_form: "Esta página ha caducado. Recárgala e inténtalo de nuevo.",
        },
    }
}

const STYLE: &str = "body{font:16px sans-serif;margin:1rem auto;max-width:28rem;padding:0 1rem}\
label{display:block;margin:1rem 0 .25rem}input[type=text],input[type=password]{box-sizing:border-box;\
font-size:1rem;padding:.5rem;width:100%}button{font-size:1rem;margin-top:1rem;padding:.5rem 1rem}\
.e{color:#b00020}";

fn response(status: &str, html: Option<String>) -> Vec<u8> {
    let body = html.unwrap_or_default();
    format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\n\
         X-Frame-Options: DENY\r\n\
         Referrer-Policy: same-origin\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; \
         form-action 'self'; base-uri 'none'; frame-ancestors 'none'\r\n\
         \r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn page(lang: Lang, inner: &str) -> String {
    let code = match lang {
        Lang::En => "en",
        Lang::It => "it",
        Lang::Fr => "fr",
        Lang::Es => "es",
    };
    let title = text(lang).title;
    format!(
        "<!doctype html><html lang=\"{code}\"><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><style>{STYLE}</style><h1>{title}</h1>{inner}</html>"
    )
}

/// The form. Every string in it is a constant of this file or the hex token.
fn form(site: &Site, problem: Option<&str>) -> String {
    let t = text(site.lang);
    let problem = problem.map_or(String::new(), |p| format!("<p class=\"e\">{p}</p>"));
    let quiet =
        "autocomplete=\"off\" autocapitalize=\"off\" autocorrect=\"off\" spellcheck=\"false\"";
    page(
        site.lang,
        &format!(
            "<p>{intro}</p>{problem}<form method=\"post\" action=\"/join\">\
             <input type=\"hidden\" name=\"token\" value=\"{token}\">\
             <label for=\"s\">{ssid}</label>\
             <input id=\"s\" type=\"text\" name=\"ssid\" maxlength=\"32\" required {quiet}>\
             <label for=\"p\">{password}</label>\
             <input id=\"p\" type=\"password\" name=\"password\" maxlength=\"63\" {quiet}>\
             <label><input type=\"checkbox\" name=\"open\" value=\"1\"> {open}</label>\
             <button type=\"submit\">{send}</button></form>",
            intro = t.intro,
            token = site.token,
            ssid = t.ssid,
            password = t.password,
            open = t.open,
            send = t.send,
        ),
    )
}

struct Head<'a> {
    method: &'a str,
    path: &'a str,
    headers: Vec<(&'a str, &'a str)>,
}

impl Head<'_> {
    /// The value of a header that must not repeat: `Err` if it does.
    fn one(&self, name: &str) -> Result<Option<&str>, ()> {
        let mut found = self
            .headers
            .iter()
            .filter(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| *v);
        let first = found.next();
        if found.next().is_some() {
            return Err(());
        }
        Ok(first)
    }
}

fn parse_head(head: &[u8]) -> Option<Head<'_>> {
    let head = std::str::from_utf8(head).ok()?;
    let mut lines = head.split("\r\n");
    let mut request = lines.next()?.split(' ');
    let (method, path, version) = (request.next()?, request.next()?, request.next()?);
    if request.next().is_some() || !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return None;
    }
    let mut headers = Vec::new();
    for line in lines {
        // A folded line, or a space before the colon, is how requests get smuggled.
        if line.starts_with([' ', '\t']) {
            return None;
        }
        let (k, v) = line.split_once(':')?;
        if k.is_empty() || k.contains([' ', '\t']) {
            return None;
        }
        headers.push((k, v.trim_matches([' ', '\t'])));
    }
    Some(Head {
        method,
        path,
        headers,
    })
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// One `application/x-www-form-urlencoded` value: `None` for a bad escape or for bytes that are
/// not UTF-8.
fn form_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hi = hex_value(*bytes.get(i + 1)?)?;
                let lo = hex_value(*bytes.get(i + 2)?)?;
                out.push(hi << 4 | lo);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Compares without stopping at the first difference.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// Checks a form body and makes the network from it.
pub fn parse_form(body: &[u8], token: &str) -> Result<Network, Refused> {
    let body = std::str::from_utf8(body).map_err(|_| Refused::Malformed)?;
    let (mut got_token, mut ssid, mut password, mut open) = (None, None, None, None);
    for pair in body.split('&') {
        let (k, v) = pair.split_once('=').ok_or(Refused::Malformed)?;
        let slot = match k {
            "token" => &mut got_token,
            "ssid" => &mut ssid,
            "password" => &mut password,
            "open" => &mut open,
            _ => return Err(Refused::Malformed),
        };
        if slot.is_some() {
            return Err(Refused::Malformed);
        }
        *slot = Some(form_decode(v).ok_or(Refused::Malformed)?);
    }
    let got_token = got_token.ok_or(Refused::Malformed)?;
    if !same(&got_token, token) {
        return Err(Refused::Token);
    }
    let ssid = ssid.ok_or(Refused::Malformed)?;
    if ssid.is_empty() || ssid.len() > 32 || ssid.chars().any(char::is_control) {
        return Err(Refused::Ssid);
    }
    let password = password.filter(|p| !p.is_empty());
    let open = match open.as_deref() {
        None => false,
        Some("1") => true,
        Some(_) => return Err(Refused::Malformed),
    };
    let (auth, psk) = match (open, password.as_deref()) {
        (true, None) => (WifiAuth::Open, None),
        (false, Some(p)) => (WifiAuth::WpaPsk, Some(p)),
        (true, Some(_)) | (false, None) => return Err(Refused::PasswordChoice),
    };
    let conf = wpa_supplicant_conf(&ssid, auth, psk).map_err(|_| Refused::Password)?;
    Ok(Network { ssid, conf })
}

fn refused_page(site: &Site, why: Refused) -> Vec<u8> {
    let t = text(site.lang);
    let (status, message) = match why {
        Refused::Malformed => ("400 Bad Request", t.bad_form),
        Refused::Token => ("403 Forbidden", t.bad_form),
        Refused::Ssid => ("400 Bad Request", t.bad_ssid),
        Refused::PasswordChoice => ("400 Bad Request", t.bad_choice),
        Refused::Password => ("400 Bad Request", t.bad_password),
    };
    response(status, Some(form(site, Some(message))))
}

fn post_join(site: &Site, head: &Head<'_>, body: &[u8], length: usize) -> Handled {
    let bad = || Handled::Respond(response("400 Bad Request", None));
    let Ok(Some(kind)) = head.one("content-type") else {
        return Handled::Respond(response("415 Unsupported Media Type", None));
    };
    let essence = kind.split(';').next().unwrap_or("").trim();
    if !essence.eq_ignore_ascii_case("application/x-www-form-urlencoded") {
        return Handled::Respond(response("415 Unsupported Media Type", None));
    }
    if body.len() > length {
        return bad();
    }
    if body.len() < length {
        return Handled::NeedMore;
    }
    match parse_form(body, &site.token) {
        Ok(net) => {
            let done = page(site.lang, &format!("<p>{}</p>", text(site.lang).joining));
            Handled::Accepted(response("200 OK", Some(done)), net)
        }
        Err(why) => Handled::Respond(refused_page(site, why)),
    }
}

/// Handles what has arrived of one request.
pub fn handle(site: &Site, input: &[u8]) -> Handled {
    let bad = || Handled::Respond(response("400 Bad Request", None));
    let Some(end) = input.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if input.len() >= MAX_REQUEST {
            Handled::Respond(response("431 Request Header Fields Too Large", None))
        } else {
            Handled::NeedMore
        };
    };
    let Some(head) = parse_head(&input[..end]) else {
        return bad();
    };
    let body = &input[end + 4..];
    // A proxy-style or rebound name is not this page.
    match head.one("host") {
        Ok(Some(h)) if h == site.host => {}
        _ => return bad(),
    }
    match head.one("origin") {
        Err(()) => return bad(),
        Ok(Some(o)) if o.strip_prefix("http://") != Some(site.host.as_str()) => {
            return Handled::Respond(refused_page(site, Refused::Token));
        }
        Ok(_) => {}
    }
    if head.one("transfer-encoding") != Ok(None) {
        return bad();
    }
    let length = match head.one("content-length") {
        Ok(None) => None,
        Ok(Some(v)) if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => {
            match v.parse::<usize>() {
                // Not a plain sum: a length near usize::MAX would wrap around and pass.
                Ok(n) if n.checked_add(end + 4).is_some_and(|t| t <= MAX_REQUEST) => Some(n),
                _ => return Handled::Respond(response("413 Content Too Large", None)),
            }
        }
        _ => return bad(),
    };
    match (head.method, head.path) {
        ("GET", "/") => {
            if !body.is_empty() || length.is_some_and(|n| n != 0) {
                return bad();
            }
            Handled::Respond(response("200 OK", Some(form(site, None))))
        }
        ("POST", "/join") => match length {
            Some(n) => post_join(site, &head, body, n),
            None => Handled::Respond(response("411 Length Required", None)),
        },
        (_, "/" | "/join") => Handled::Respond(response("405 Method Not Allowed", None)),
        _ => Handled::Respond(response("404 Not Found", None)),
    }
}

/// The answer while the session is already closing on a form it accepted.
pub fn closing() -> Vec<u8> {
    response("409 Conflict", None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "00112233445566778899aabbccddeeff";

    fn site() -> Site {
        Site {
            host: "192.168.77.1".into(),
            token: TOKEN.into(),
            lang: Lang::En,
        }
    }

    fn post(body: &str) -> Vec<u8> {
        format!(
            "POST /join HTTP/1.1\r\nHost: 192.168.77.1\r\n\
             Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
        .into_bytes()
    }

    fn status(h: &Handled) -> String {
        let bytes = match h {
            Handled::Respond(b) | Handled::Accepted(b, _) => b,
            Handled::NeedMore => return "need more".into(),
        };
        let s = String::from_utf8_lossy(bytes);
        s.lines().next().unwrap().to_string()
    }

    fn body_of(h: &Handled) -> String {
        match h {
            Handled::Respond(b) | Handled::Accepted(b, _) => String::from_utf8_lossy(b).to_string(),
            Handled::NeedMore => String::new(),
        }
    }

    /// `Referrer-Policy: no-referrer` makes browsers send `Origin: null` on the page's own form,
    /// and that is refused. Sent by Firefox 159 for a form on a page with these headers.
    #[test]
    fn a_form_from_a_real_browser_is_accepted() {
        const CAPTURED: &str = "POST /join HTTP/1.1\r\n\
            Host: 127.0.0.1:18081\r\n\
            User-Agent: Mozilla/5.0 (X11; Linux x86_64; rv:159.0) Gecko/20100101 Firefox/159.0\r\n\
            Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8\r\n\
            Accept-Language: it-IT,it;q=0.9,en-US;q=0.8,en;q=0.7\r\n\
            Accept-Encoding: gzip, deflate, br, zstd\r\n\
            Referer: http://127.0.0.1:18081/\r\n\
            Content-Type: application/x-www-form-urlencoded\r\n\
            Content-Length: 36\r\n\
            Origin: http://127.0.0.1:18081\r\n\
            Connection: keep-alive\r\n\
            Upgrade-Insecure-Requests: 1\r\n\
            Sec-Fetch-Dest: document\r\n\
            Sec-Fetch-Mode: navigate\r\n\
            Sec-Fetch-Site: same-origin\r\n\
            Priority: u=0, i\r\n\
            \r\n\
            token=abcd&ssid=N&password=password1";
        let s = Site {
            host: "127.0.0.1:18081".into(),
            token: "abcd".into(),
            lang: Lang::En,
        };
        assert!(matches!(
            handle(&s, CAPTURED.as_bytes()),
            Handled::Accepted(..)
        ));
        let page = body_of(&handle(
            &s,
            b"GET / HTTP/1.1\r\nHost: 127.0.0.1:18081\r\n\r\n",
        ));
        assert!(page.contains("Referrer-Policy: same-origin"));
        assert!(
            !page.contains("no-referrer"),
            "it would bring back Origin: null"
        );
    }

    #[test]
    fn a_refused_origin_gets_a_page_saying_so() {
        let mut req =
            String::from_utf8(post(&format!("token={TOKEN}&ssid=N&password=password"))).unwrap();
        req = req.replacen("\r\n", "\r\nOrigin: null\r\n", 1);
        let h = handle(&site(), req.as_bytes());
        assert_eq!(status(&h), "HTTP/1.1 403 Forbidden");
        assert!(body_of(&h).contains("expired"));
    }

    #[test]
    fn a_length_that_would_wrap_around_is_refused() {
        for n in [u64::MAX, u64::from(u32::MAX), u64::MAX - 100] {
            let r = format!(
                "POST /join HTTP/1.1\r\nHost: 192.168.77.1\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {n}\r\n\r\n"
            );
            assert_eq!(
                status(&handle(&site(), r.as_bytes())),
                "HTTP/1.1 413 Content Too Large"
            );
        }
    }

    #[test]
    fn a_full_buffer_always_gets_an_answer() {
        // Whatever the split between head and body: the portal stops reading at this size, so
        // waiting for more here would leave the connection ready and never read.
        let head = |n: usize| {
            format!(
                "POST /join HTTP/1.1\r\nHost: 192.168.77.1\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {n}\r\n\r\n"
            )
        };
        for n in [1, 100, 3000, MAX_REQUEST - 1, MAX_REQUEST] {
            let mut input = head(n).into_bytes();
            input.resize(MAX_REQUEST, b'a');
            assert_ne!(handle(&site(), &input), Handled::NeedMore, "{n}");
        }
    }

    #[test]
    fn the_page_has_the_token_and_locks_itself_down() {
        let h = handle(&site(), b"GET / HTTP/1.1\r\nHost: 192.168.77.1\r\n\r\n");
        assert_eq!(status(&h), "HTTP/1.1 200 OK");
        let text = body_of(&h);
        assert!(text.contains(TOKEN));
        assert!(text.contains("default-src 'none'"));
        assert!(text.contains("no-store"));
        assert!(text.contains("DENY"));
        assert!(!text.contains("<script"));
        assert!(
            !text.contains("http://"),
            "the page loads nothing from anywhere"
        );
    }

    #[test]
    fn a_good_form_gives_a_private_conf_that_does_not_show_in_debug() {
        let h = handle(
            &site(),
            &post(&format!(
                "token={TOKEN}&ssid=My+Net%21&password=sup%26r-secret"
            )),
        );
        let Handled::Accepted(_, net) = &h else {
            panic!("{}", status(&h));
        };
        assert_eq!(net.ssid, "My Net!");
        assert!(net.conf.contains("psk=\"sup&r-secret\""));
        let shown = format!("{h:?}");
        // The response bytes are printed as numbers, and the network is redacted.
        assert!(!shown.contains("secret") && !shown.contains("My Net"));
    }

    #[test]
    fn an_open_network_needs_the_box_and_no_password() {
        let open = |b: &str| parse_form(format!("token={TOKEN}&ssid=Cafe{b}").as_bytes(), TOKEN);
        assert!(open("&open=1").unwrap().conf.contains("key_mgmt=NONE"));
        assert!(
            open("&open=1&password=")
                .unwrap()
                .conf
                .contains("key_mgmt=NONE")
        );
        assert_eq!(open(""), Err(Refused::PasswordChoice));
        assert_eq!(open("&password="), Err(Refused::PasswordChoice));
        assert_eq!(
            open("&open=1&password=password1"),
            Err(Refused::PasswordChoice)
        );
        assert_eq!(open("&open=yes"), Err(Refused::Malformed));
    }

    #[test]
    fn a_form_cannot_add_config_lines() {
        for evil in ["a%0Ab", "a%0Db", "a%00b", "a%09b", "a%7Fb", "%C2%85abc"] {
            let f = format!("token={TOKEN}&ssid={evil}&password=password1");
            assert_eq!(
                parse_form(f.as_bytes(), TOKEN),
                Err(Refused::Ssid),
                "{evil}"
            );
        }
        for evil in [
            "passw%0Aord1",
            "passw%0Dord1",
            "passw%00ord1",
            "passw%09ord1",
        ] {
            let f = format!("token={TOKEN}&ssid=Net&password={evil}");
            assert_eq!(
                parse_form(f.as_bytes(), TOKEN),
                Err(Refused::Password),
                "{evil}"
            );
        }
        // The quoted form takes these as they are.
        let f = format!("token={TOKEN}&ssid=%22%7D%0A&password=password1");
        assert_eq!(parse_form(f.as_bytes(), TOKEN), Err(Refused::Ssid));
        let f = format!("token={TOKEN}&ssid=%22%7D&password=pass%22word");
        let net = parse_form(f.as_bytes(), TOKEN).unwrap();
        assert!(net.conf.contains("ssid=227d\n"), "the name goes in hex");
        assert_eq!(net.conf.matches("network={").count(), 1);
    }

    #[test]
    fn lengths_are_enforced_in_bytes() {
        let name = |n: usize| "a".repeat(n);
        let with = |s: &str, p: &str| format!("token={TOKEN}&ssid={s}&password={p}");
        let ok = |f: String| parse_form(f.as_bytes(), TOKEN).map(|_| ());
        assert_eq!(ok(with(&name(32), "password")), Ok(()));
        assert_eq!(ok(with(&name(33), "password")), Err(Refused::Ssid));
        assert_eq!(ok(with("", "password")), Err(Refused::Ssid));
        assert_eq!(ok(with("n", &name(7))), Err(Refused::Password));
        assert_eq!(ok(with("n", &name(63))), Ok(()));
        assert_eq!(ok(with("n", &name(64))), Err(Refused::Password));
        // 11 three-byte characters are 33 bytes.
        assert_eq!(
            ok(with(&"%E2%82%AC".repeat(11), "password")),
            Err(Refused::Ssid)
        );
    }

    #[test]
    fn a_malformed_form_is_refused() {
        for bad in [
            "",
            "ssid=a",
            "token",
            "&&&",
            &format!("token={TOKEN}&ssid=a&ssid=b&password=password"),
            &format!("token={TOKEN}&ssid=a&password=password&extra=1"),
            &format!("token={TOKEN}&ssid=%G1&password=password"),
            &format!("token={TOKEN}&ssid=%E2%82&password=password"),
            &format!("token={TOKEN}&ssid=a%2&password=password"),
        ] {
            assert_eq!(
                parse_form(bad.as_bytes(), TOKEN),
                Err(Refused::Malformed),
                "{bad}"
            );
        }
        assert_eq!(parse_form(&[0xff, b'='], TOKEN), Err(Refused::Malformed));
    }

    #[test]
    fn a_form_from_another_page_has_no_token() {
        let h = handle(&site(), &post("token=00&ssid=Net&password=password"));
        assert_eq!(status(&h), "HTTP/1.1 403 Forbidden");
        let wrong = "f".repeat(TOKEN.len());
        assert_eq!(
            parse_form(
                format!("token={wrong}&ssid=N&password=password").as_bytes(),
                TOKEN
            ),
            Err(Refused::Token)
        );
    }

    #[test]
    fn nothing_typed_comes_back_in_a_page() {
        let evil = "%3Cscript%3Ealert(1)%3C%2Fscript%3E";
        let h = handle(
            &site(),
            &post(&format!("token={TOKEN}&ssid={evil}{evil}&password=hunter2")),
        );
        let text = body_of(&h);
        assert!(status(&h).contains("400"));
        assert!(!text.contains("alert") && !text.contains("hunter2"));
    }

    #[test]
    fn a_rebound_name_or_a_foreign_origin_is_refused() {
        for req in [
            "GET / HTTP/1.1\r\nHost: evil.example\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 192.168.77.1:8080\r\n\r\n",
            "GET / HTTP/1.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 192.168.77.1\r\nHost: evil.example\r\n\r\n",
        ] {
            assert_eq!(
                status(&handle(&site(), req.as_bytes())),
                "HTTP/1.1 400 Bad Request"
            );
        }
        let mut req =
            String::from_utf8(post(&format!("token={TOKEN}&ssid=N&password=password"))).unwrap();
        req = req.replacen("\r\n", "\r\nOrigin: http://evil.example\r\n", 1);
        assert_eq!(
            status(&handle(&site(), req.as_bytes())),
            "HTTP/1.1 403 Forbidden"
        );
        let same = req.replace("http://evil.example", "http://192.168.77.1");
        assert!(matches!(
            handle(&site(), same.as_bytes()),
            Handled::Accepted(..)
        ));
    }

    #[test]
    fn smuggling_shapes_are_refused() {
        for req in [
            "GET / HTTP/1.1\r\nHost: 192.168.77.1\r\n folded: x\r\n\r\n",
            "GET / HTTP/1.1\r\nHost : 192.168.77.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 192.168.77.1\r\nno colon\r\n\r\n",
            "GET / extra HTTP/1.1\r\nHost: 192.168.77.1\r\n\r\n",
            "GET / HTTP/2\r\nHost: 192.168.77.1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 192.168.77.1\r\nTransfer-Encoding: chunked\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 192.168.77.1\r\nContent-Length: 1\r\nContent-Length: 2\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 192.168.77.1\r\nContent-Length: -1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 192.168.77.1\r\nContent-Length: +1\r\n\r\n",
            "GET / HTTP/1.1\r\nHost: 192.168.77.1\r\n\r\nextra",
        ] {
            assert_eq!(
                status(&handle(&site(), req.as_bytes())),
                "HTTP/1.1 400 Bad Request",
                "{req}"
            );
        }
        assert_eq!(
            status(&handle(&site(), &[0xff, 0xff, b'\r', b'\n', b'\r', b'\n'])),
            "HTTP/1.1 400 Bad Request"
        );
    }

    #[test]
    fn only_the_two_routes_exist() {
        let get = |p: &str| {
            let r = format!("GET {p} HTTP/1.1\r\nHost: 192.168.77.1\r\n\r\n");
            status(&handle(&site(), r.as_bytes()))
        };
        assert_eq!(get("/join"), "HTTP/1.1 405 Method Not Allowed");
        for p in [
            "/?a=1",
            "/../etc/passwd",
            "/favicon.ico",
            "http://evil.example/",
        ] {
            assert_eq!(get(p), "HTTP/1.1 404 Not Found");
        }
        let r = b"POST / HTTP/1.1\r\nHost: 192.168.77.1\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(
            status(&handle(&site(), r)),
            "HTTP/1.1 405 Method Not Allowed"
        );
    }

    #[test]
    fn a_post_needs_a_length_and_the_form_type() {
        let r = b"POST /join HTTP/1.1\r\nHost: 192.168.77.1\r\n\r\n";
        assert_eq!(status(&handle(&site(), r)), "HTTP/1.1 411 Length Required");
        let r = b"POST /join HTTP/1.1\r\nHost: 192.168.77.1\r\nContent-Type: text/plain\r\nContent-Length: 1\r\n\r\nx";
        assert_eq!(
            status(&handle(&site(), r)),
            "HTTP/1.1 415 Unsupported Media Type"
        );
        let r = b"POST /join HTTP/1.1\r\nHost: 192.168.77.1\r\nContent-Length: 1\r\n\r\nx";
        assert_eq!(
            status(&handle(&site(), r)),
            "HTTP/1.1 415 Unsupported Media Type"
        );
    }

    #[test]
    fn a_request_can_arrive_in_pieces_and_cannot_grow_without_limit() {
        let whole = post(&format!("token={TOKEN}&ssid=N&password=password"));
        for cut in 0..whole.len() {
            assert_eq!(
                handle(&site(), &whole[..cut]),
                Handled::NeedMore,
                "cut {cut}"
            );
        }
        assert!(matches!(handle(&site(), &whole), Handled::Accepted(..)));
        let mut extra = whole.clone();
        extra.push(b'x');
        assert_eq!(status(&handle(&site(), &extra)), "HTTP/1.1 400 Bad Request");

        let endless = vec![b'a'; MAX_REQUEST];
        assert_eq!(
            status(&handle(&site(), &endless)),
            "HTTP/1.1 431 Request Header Fields Too Large"
        );
        let big = format!(
            "POST /join HTTP/1.1\r\nHost: 192.168.77.1\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {MAX_REQUEST}\r\n\r\n"
        );
        assert_eq!(
            status(&handle(&site(), big.as_bytes())),
            "HTTP/1.1 413 Content Too Large"
        );
        let huge = "POST /join HTTP/1.1\r\nHost: 192.168.77.1\r\nContent-Length: 99999999999999999999999\r\n\r\n";
        assert_eq!(
            status(&handle(&site(), huge.as_bytes())),
            "HTTP/1.1 413 Content Too Large"
        );
    }

    #[test]
    fn every_language_has_its_page() {
        for (lang, word) in [
            (Lang::En, "Network name"),
            (Lang::It, "Nome della rete"),
            (Lang::Fr, "Nom du réseau"),
            (Lang::Es, "Nombre de la red"),
        ] {
            let mut s = site();
            s.lang = lang;
            let h = handle(&s, b"GET / HTTP/1.1\r\nHost: 192.168.77.1\r\n\r\n");
            assert!(body_of(&h).contains(word));
        }
    }
}
