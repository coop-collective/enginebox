//! Room principals: a connected app (Gamejam Account) vouching, per request,
//! for the signed-in person it acts for (docs/ENGINE-SESSIONS.md, "Room
//! principals"; owner decision 2026-11-04 #2).
//!
//! Most room members are not Engine Box org members, so the room owner's
//! `agent-forks` grant admits **app-asserted** principals `gj:<room>:<user>`,
//! only on agent-fork routes of the grant's project. Each request carries:
//!
//! ```text
//! EngineBox-Principal:       gj:<room>:<user>
//! EngineBox-Principal-Name:  <display name, percent-encoded UTF-8>
//! EngineBox-Principal-Role:  builder | viewer
//! EngineBox-Principal-Time:  <unix seconds>
//! EngineBox-Principal-Nonce: <16-64 of A-Z a-z 0-9 _ ->
//! EngineBox-Principal-Sig:   <base64url Ed25519 signature, no padding>
//! ```
//!
//! The signature covers, joined with `\n`: `enginebox-principal-v1`, the
//! method, the path, the raw query (empty when none), the hex SHA-256 of the
//! body (of the empty string when none), the principal, the raw name header,
//! the role, the time, the nonce and the access token's id (`ebst_<id>.…`),
//! so it cannot move to another request, body, grant or token.
//!
//! The key is the app's Ed25519 key pair: the public half is registered on
//! the OAuth client (`enginebox_principal_key`) and copied into every access
//! token minted for the grant; Engine Box never holds anything that can sign.
//! Requests more than 60 s from the controller's clock are refused, and each
//! nonce is accepted once per key within that window (an in-memory cache, so
//! a restart forgets nonces older than the restart, still bounded by the
//! window).
use anyhow::Result;
use base64::Engine as _;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

pub const HEADER: &str = "enginebox-principal";
pub const NAME_HEADER: &str = "enginebox-principal-name";
pub const ROLE_HEADER: &str = "enginebox-principal-role";
pub const TIME_HEADER: &str = "enginebox-principal-time";
pub const NONCE_HEADER: &str = "enginebox-principal-nonce";
pub const SIG_HEADER: &str = "enginebox-principal-sig";
pub const HEADERS: [&str; 6] = [
    HEADER,
    NAME_HEADER,
    ROLE_HEADER,
    TIME_HEADER,
    NONCE_HEADER,
    SIG_HEADER,
];
pub const SKEW_SECONDS: u64 = 60;
const MAX_NONCES: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Builder,
    Viewer,
}

/// A room member the app vouched for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Room {
    /// The OAuth client (`ebc_…`) that vouched: principals of different apps
    /// never meet.
    pub client: String,
    pub room: String,
    pub user: String,
    pub name: String,
    pub role: Role,
}
impl Room {
    /// `gj:<room>:<user>`, as callers see it.
    pub fn principal(&self) -> String {
        format!("gj:{}:{}", self.room, self.user)
    }
}

/// The asserted request as received (raw header values).
pub struct Assertion<'a> {
    pub principal: &'a str,
    pub name: &'a str,
    pub role: &'a str,
    pub time: &'a str,
    pub nonce: &'a str,
    pub signature: &'a str,
}

/// What the signature is bound to besides the headers.
pub struct Bound<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub body: &'a [u8],
    pub token_id: &'a str,
}

fn segment(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `gj:<room>:<user>` → (room, user).
pub fn parse(principal: &str) -> Option<(&str, &str)> {
    let rest = principal.strip_prefix("gj:")?;
    let (room, user) = rest.split_once(':')?;
    (segment(room) && segment(user)).then_some((room, user))
}

/// Percent-decode a display name: 1-64 characters, no control characters.
pub fn decode_name(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.len() > 512 || !raw.bytes().all(|b| (0x21..0x7f).contains(&b)) {
        return None;
    }
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    let name = String::from_utf8(out).ok()?;
    let name = name.trim().to_owned();
    (!name.is_empty() && name.chars().count() <= 64 && !name.chars().any(char::is_control))
        .then_some(name)
}

/// A registered public key: base64url (no padding) of 32 bytes.
pub fn valid_key(key: &str) -> bool {
    key.len() == 43
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(key)
            .is_ok_and(|k| k.len() == 32)
}

/// The exact bytes the app signs.
pub fn message(a: &Assertion, b: &Bound) -> String {
    let body = format!("{:x}", Sha256::digest(b.body));
    [
        "enginebox-principal-v1",
        b.method,
        b.path,
        b.query,
        &body,
        a.principal,
        a.name,
        a.role,
        a.time,
        a.nonce,
        b.token_id,
    ]
    .join("\n")
}

/// Why an assertion was refused (the caller sees the message; HTTP 401).
#[derive(Debug, PartialEq, Eq)]
pub struct Refused(pub &'static str);
impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Refused {}

/// Accepted nonces per key until they leave the clock window.
#[derive(Clone, Default)]
pub struct Verifier {
    seen: Arc<Mutex<BTreeMap<(String, String), u64>>>,
}
impl Verifier {
    /// Verify `a` against the grant's registered `key` at unix time `now`.
    pub fn verify(
        &self,
        key: &str,
        client: &str,
        a: &Assertion,
        b: &Bound,
        now: u64,
    ) -> Result<Room, Refused> {
        let (room, user) = parse(a.principal).ok_or(Refused(
            "EngineBox-Principal must be gj:<room>:<user> (1-64 letters, digits, _ or - each).",
        ))?;
        let name = decode_name(a.name).ok_or(Refused(
            "EngineBox-Principal-Name must be a percent-encoded name of 1-64 characters.",
        ))?;
        let role = match a.role {
            "builder" => Role::Builder,
            "viewer" => Role::Viewer,
            _ => {
                return Err(Refused(
                    "EngineBox-Principal-Role must be builder or viewer.",
                ));
            }
        };
        let time: u64 = a
            .time
            .parse()
            .ok()
            .filter(|_| a.time.len() <= 12 && a.time.bytes().all(|b| b.is_ascii_digit()))
            .ok_or(Refused("EngineBox-Principal-Time must be unix seconds."))?;
        if time.abs_diff(now) > SKEW_SECONDS {
            return Err(Refused(
                "The principal assertion is too old or from the future (more than 60 s from Engine Box's clock).",
            ));
        }
        if a.nonce.len() < 16 || !segment(a.nonce) {
            return Err(Refused(
                "EngineBox-Principal-Nonce must be 16-64 letters, digits, _ or -.",
            ));
        }
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(a.signature)
            .ok()
            .filter(|s| s.len() == 64)
            .ok_or(Refused(
                "EngineBox-Principal-Sig must be a base64url Ed25519 signature.",
            ))?;
        let public = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(key)
            .map_err(|_| Refused("The connection's principal key is invalid."))?;
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, &public)
            .verify(message(a, b).as_bytes(), &signature)
            .map_err(|_| Refused("The principal assertion's signature does not verify."))?;
        // Replay: each nonce once per key while it could still be accepted.
        let mut seen = self.seen.lock().unwrap();
        seen.retain(|_, at| at.saturating_add(2 * SKEW_SECONDS) > now);
        let entry = (key.to_owned(), a.nonce.to_owned());
        if seen.contains_key(&entry) {
            return Err(Refused("The principal assertion was already used."));
        }
        if seen.len() >= MAX_NONCES {
            return Err(Refused(
                "Too many principal assertions right now; retry shortly.",
            ));
        }
        seen.insert(entry, time.max(now));
        Ok(Room {
            client: client.to_owned(),
            room: room.to_owned(),
            user: user.to_owned(),
            name,
            role,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// A fixed test key pair (the app's private half) and its public key.
    pub fn keypair() -> (Ed25519KeyPair, String) {
        let pair = Ed25519KeyPair::from_seed_unchecked(&[7u8; 32]).unwrap();
        let public = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pair.public_key());
        (pair, public)
    }
    /// Sign as the app would; returns the six headers.
    pub fn sign(
        pair: &Ed25519KeyPair,
        principal: &str,
        role: &str,
        time: u64,
        nonce: &str,
        b: &Bound,
    ) -> Vec<(&'static str, String)> {
        let name = "Ada%20L%C3%B6vel";
        let t = time.to_string();
        let a = Assertion {
            principal,
            name,
            role,
            time: &t,
            nonce,
            signature: "",
        };
        let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(pair.sign(message(&a, b).as_bytes()).as_ref());
        vec![
            (HEADER, principal.to_owned()),
            (NAME_HEADER, name.to_owned()),
            (ROLE_HEADER, role.to_owned()),
            (TIME_HEADER, t),
            (NONCE_HEADER, nonce.to_owned()),
            (SIG_HEADER, sig),
        ]
    }
    fn assertion<'a>(h: &'a [(&'static str, String)]) -> Assertion<'a> {
        let get = |n: &str| h.iter().find(|(k, _)| *k == n).unwrap().1.as_str();
        Assertion {
            principal: get(HEADER),
            name: get(NAME_HEADER),
            role: get(ROLE_HEADER),
            time: get(TIME_HEADER),
            nonce: get(NONCE_HEADER),
            signature: get(SIG_HEADER),
        }
    }

    #[test]
    fn verifies_once_and_binds_everything() {
        let (pair, key) = keypair();
        let v = Verifier::default();
        let bound = Bound {
            method: "POST",
            path: "/v1/projects/qoh/engine-sessions/x/forks",
            query: "",
            body: b"{\"prompt\":\"hi\"}",
            token_id: "0123",
        };
        let now = 1_800_000_000;
        let h = sign(
            &pair,
            "gj:room-1:user_7",
            "builder",
            now,
            "nonce-0123456789abcdef",
            &bound,
        );
        let room = v
            .verify(&key, "ebc_app", &assertion(&h), &bound, now + 5)
            .unwrap();
        assert_eq!(room.principal(), "gj:room-1:user_7");
        assert_eq!(room.name, "Ada Lövel");
        assert_eq!(room.role, Role::Builder);
        // Replay of the same nonce.
        assert_eq!(
            v.verify(&key, "ebc_app", &assertion(&h), &bound, now + 6),
            Err(Refused("The principal assertion was already used."))
        );
        // Any bound field changed: the signature fails.
        let h2 = sign(
            &pair,
            "gj:room-1:user_7",
            "builder",
            now,
            "nonce-other-123456789",
            &bound,
        );
        for other in [
            Bound {
                body: b"{}",
                ..bound
            },
            Bound {
                path: "/v1/projects/qoh/engine-sessions/y/forks",
                ..bound
            },
            Bound {
                token_id: "9999",
                ..bound
            },
            Bound {
                method: "GET",
                ..bound
            },
            Bound {
                query: "all=1",
                ..bound
            },
        ] {
            assert!(
                v.verify(&key, "ebc_app", &assertion(&h2), &other, now)
                    .is_err()
            );
        }
        // Too old, wrong key, bad shapes.
        let old = sign(
            &pair,
            "gj:room-1:user_7",
            "builder",
            now - 61,
            "nonce-old-0123456789",
            &bound,
        );
        assert!(
            v.verify(&key, "ebc_app", &assertion(&old), &bound, now)
                .is_err()
        );
        let other = Ed25519KeyPair::from_seed_unchecked(&[9u8; 32]).unwrap();
        let forged = sign(
            &other,
            "gj:room-1:user_7",
            "builder",
            now,
            "nonce-forged-01234567",
            &bound,
        );
        assert!(
            v.verify(&key, "ebc_app", &assertion(&forged), &bound, now)
                .is_err()
        );
        let bad = sign(
            &pair,
            "usr_x",
            "builder",
            now,
            "nonce-bad-principal-1",
            &bound,
        );
        assert!(
            v.verify(&key, "ebc_app", &assertion(&bad), &bound, now)
                .is_err()
        );
        let role = sign(
            &pair,
            "gj:r:u",
            "owner",
            now,
            "nonce-bad-role-012345",
            &bound,
        );
        assert!(
            v.verify(&key, "ebc_app", &assertion(&role), &bound, now)
                .is_err()
        );
    }

    #[test]
    fn names_and_keys_are_checked() {
        assert_eq!(decode_name("Bob").as_deref(), Some("Bob"));
        assert_eq!(decode_name("%E2%9C%93%20ok").as_deref(), Some("✓ ok"));
        assert!(decode_name("").is_none());
        assert!(decode_name("a b").is_none());
        assert!(decode_name("%0A").is_none());
        assert!(decode_name("%ZZ").is_none());
        assert!(decode_name(&"a".repeat(65)).is_none());
        assert!(valid_key(&keypair().1));
        assert!(!valid_key("short"));
        assert_eq!(parse("gj:r:u"), Some(("r", "u")));
        assert!(parse("gj:r").is_none() && parse("gj:r:u:v").is_none());
    }
}
