//! The built-in validators a rule can name with `validate:`.
//!
//! A pattern alone is a poor detector for structured numbers: thirteen to
//! nineteen digits are a card number only when the check digit agrees, and
//! two letters plus digits are an IBAN only when mod-97 says so. Requiring
//! the checksum is what keeps order ids and timestamps out of the findings.
//! None of these look anything up (no DNS, no network): a validator sees
//! one string and answers yes or no.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A validator a rule may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Validator {
    /// [`luhn`].
    Luhn,
    /// [`iban`].
    Iban,
    /// [`email_mx_shape`].
    EmailMxShape,
}

impl Validator {
    /// Every name a rules file may use, for error messages.
    pub const NAMES: &'static str = "luhn, iban, email-mx-shape";

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "luhn" => Some(Validator::Luhn),
            "iban" => Some(Validator::Iban),
            "email-mx-shape" => Some(Validator::EmailMxShape),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Validator::Luhn => "luhn",
            Validator::Iban => "iban",
            Validator::EmailMxShape => "email-mx-shape",
        }
    }

    pub fn check(self, text: &str) -> bool {
        match self {
            Validator::Luhn => luhn(text),
            Validator::Iban => iban(text),
            Validator::EmailMxShape => email_mx_shape(text),
        }
    }
}

impl fmt::Display for Validator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A payment card number: 13 to 19 ASCII digits once spaces and dashes are
/// removed, passing the Luhn check. Any other character fails it. An all-zero
/// number passes the arithmetic but is a placeholder, never a card, so it
/// fails here rather than flagging every zero-filled fixture column.
pub fn luhn(text: &str) -> bool {
    let mut digits = Vec::with_capacity(19);
    for c in text.trim().chars() {
        match c {
            ' ' | '-' => {}
            '0'..='9' => {
                if digits.len() == 19 {
                    return false;
                }
                digits.push(c as u8 - b'0');
            }
            _ => return false,
        }
    }
    if !(13..=19).contains(&digits.len()) || digits.iter().all(|d| *d == 0) {
        return false;
    }
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, d)| {
            let d = u32::from(*d);
            if i % 2 == 1 {
                let x = d * 2;
                if x > 9 {
                    x - 9
                } else {
                    x
                }
            } else {
                d
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// IBAN lengths for the countries most likely to show up. A country outside
/// the table is still checked (2 letters, 2 digits, 15 to 34 characters,
/// mod-97): missing a real account number is worse than the one-in-97
/// chance of a look-alike passing.
const IBAN_LENGTHS: &[(&str, usize)] = &[
    ("AD", 24),
    ("AE", 23),
    ("AT", 20),
    ("BE", 16),
    ("BG", 22),
    ("BH", 22),
    ("BR", 29),
    ("CH", 21),
    ("CY", 28),
    ("CZ", 24),
    ("DE", 22),
    ("DK", 18),
    ("EE", 20),
    ("ES", 24),
    ("FI", 18),
    ("FO", 18),
    ("FR", 27),
    ("GB", 22),
    ("GI", 23),
    ("GL", 18),
    ("GR", 27),
    ("HR", 21),
    ("HU", 28),
    ("IE", 22),
    ("IL", 23),
    ("IS", 26),
    ("IT", 27),
    ("KW", 30),
    ("KZ", 20),
    ("LB", 28),
    ("LI", 21),
    ("LT", 20),
    ("LU", 20),
    ("LV", 21),
    ("MC", 27),
    ("ME", 22),
    ("MK", 19),
    ("MT", 31),
    ("MU", 30),
    ("NL", 18),
    ("NO", 15),
    ("PK", 24),
    ("PL", 28),
    ("PT", 25),
    ("QA", 29),
    ("RO", 24),
    ("RS", 22),
    ("SA", 24),
    ("SE", 24),
    ("SI", 19),
    ("SK", 24),
    ("SM", 27),
    ("TN", 24),
    ("TR", 26),
    ("UA", 29),
    ("VA", 22),
    ("XK", 20),
];

/// An international bank account number: a country code, two check digits
/// and an alphanumeric account part, with the country's length (where the
/// table knows it) and a mod-97 remainder of 1. Spaces are ignored and
/// letters may be either case, matching how IBANs are printed and typed.
pub fn iban(text: &str) -> bool {
    let compact: Vec<u8> = text
        .trim()
        .bytes()
        .filter(|b| *b != b' ')
        .map(|b| b.to_ascii_uppercase())
        .collect();
    if !(15..=34).contains(&compact.len()) {
        return false;
    }
    if !compact[..2].iter().all(u8::is_ascii_uppercase)
        || !compact[2..4].iter().all(u8::is_ascii_digit)
        || !compact[4..].iter().all(u8::is_ascii_alphanumeric)
    {
        return false;
    }
    let country = std::str::from_utf8(&compact[..2]).unwrap_or("");
    if let Some((_, len)) = IBAN_LENGTHS.iter().find(|(c, _)| *c == country) {
        if compact.len() != *len {
            return false;
        }
    }
    // Move the first four characters to the end, read letters as 10..35,
    // and take the remainder digit by digit so nothing overflows.
    let mut rem: u32 = 0;
    for b in compact[4..].iter().chain(&compact[..4]) {
        let v = if b.is_ascii_digit() {
            u32::from(b - b'0')
        } else {
            u32::from(b - b'A') + 10
        };
        rem = if v >= 10 {
            (rem * 100 + v) % 97
        } else {
            (rem * 10 + v) % 97
        };
    }
    rem == 1
}

/// Top-level domains that never route mail: reserved for testing and
/// documentation, or local-network only.
const NON_MAIL_TLDS: &[&str] = &[
    "test",
    "example",
    "invalid",
    "localhost",
    "local",
    "internal",
    "onion",
    "alt",
];

/// Second-level domains reserved for documentation (they publish no usable
/// mail exchanger), with every subdomain under them.
const DOCUMENTATION_DOMAINS: &[&str] = &["example.com", "example.net", "example.org"];

/// An address that could receive mail somewhere: `local@domain.tld` with a
/// plausible local part, DNS-shaped labels, an alphabetic (or punycode) TLD,
/// and neither a reserved test TLD nor a documentation domain. Nothing is
/// looked up; this is shape only, which is what separates real customer
/// addresses from the fixtures dev data is meant to hold.
pub fn email_mx_shape(text: &str) -> bool {
    let text = text.trim();
    let Some((local, domain)) = text.rsplit_once('@') else {
        return false;
    };
    if local.is_empty()
        || local.len() > 64
        || local.starts_with('.')
        || local.ends_with('.')
        || local.contains("..")
        || !local
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "!#$%&'*+/=?^_`{|}~.-".contains(c))
    {
        return false;
    }
    let domain = domain.to_ascii_lowercase();
    if domain.len() > 253 {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    for label in &labels {
        if label.is_empty()
            || label.len() > 63
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return false;
        }
    }
    let tld = labels[labels.len() - 1];
    let tld_ok = ((2..=24).contains(&tld.len()) && tld.chars().all(|c| c.is_ascii_lowercase()))
        || (tld.len() > 4 && tld.starts_with("xn--"));
    if !tld_ok || NON_MAIL_TLDS.contains(&tld) {
        return false;
    }
    !DOCUMENTATION_DOMAINS
        .iter()
        .any(|d| domain == *d || domain.ends_with(&format!(".{d}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn luhn_vectors() {
        let cases: &[(&str, bool)] = &[
            // the published test card numbers
            ("4111111111111111", true),
            ("4111 1111 1111 1111", true),
            ("4111-1111-1111-1111", true),
            ("5555555555554444", true),
            ("378282246310005", true),
            ("6011111111111117", true),
            ("4012888888881881", true),
            ("4222222222222", true),
            ("3530111333300000", true),
            // one digit off
            ("4111111111111112", false),
            ("5555555555554443", false),
            // Luhn-valid but too short or too long for a card
            ("79927398713", false),
            ("41111111111111111111", false),
            // not a number at all
            ("4111a11111111111", false),
            ("4111.1111.1111.1111", false),
            ("", false),
            ("0000000000000000", false),
            ("  4111111111111111  ", true),
        ];
        for (text, want) in cases {
            assert_eq!(luhn(text), *want, "{text:?}");
        }
    }

    #[test]
    fn iban_vectors() {
        let cases: &[(&str, bool)] = &[
            // the published example IBANs
            ("GB82 WEST 1234 5698 7654 32", true),
            ("GB82WEST12345698765432", true),
            ("gb82 west 1234 5698 7654 32", true),
            ("DE89 3704 0044 0532 0130 00", true),
            ("FR14 2004 1010 0505 0001 3M02 606", true),
            ("NL91 ABNA 0417 1643 00", true),
            ("BE68 5390 0754 7034", true),
            ("CH93 0076 2011 6238 5295 7", true),
            ("NO93 8601 1117 947", true),
            // wrong check digits
            ("GB82 WEST 1234 5698 7654 33", false),
            ("GB83 WEST 1234 5698 7654 32", false),
            ("DE88 3704 0044 0532 0130 00", false),
            // right checksum shape, wrong length for the country
            ("GB82 WEST 1234 5698 7654 3", false),
            ("NL91 ABNA 0417 1643 001", false),
            // malformed
            ("1282 WEST 1234 5698 7654 32", false),
            ("GBXX WEST 1234 5698 7654 32", false),
            ("GB82-WEST-1234-5698-7654-32", false),
            ("GB82", false),
            ("", false),
        ];
        for (text, want) in cases {
            assert_eq!(iban(text), *want, "{text:?}");
        }
    }

    #[test]
    fn iban_unknown_country_is_still_checked_by_mod97() {
        // Build a valid-checksum IBAN for a country not in the table by
        // computing the check digits the standard way.
        let body = "ZZ00ABCD1234567890";
        let mut rem = 0u32;
        for b in body[4..].bytes().chain(body[..4].bytes()) {
            let v = if b.is_ascii_digit() {
                u32::from(b - b'0')
            } else {
                u32::from(b - b'A') + 10
            };
            rem = if v >= 10 {
                (rem * 100 + v) % 97
            } else {
                (rem * 10 + v) % 97
            };
        }
        let check = 98 - rem;
        let valid = format!("ZZ{check:02}ABCD1234567890");
        assert!(iban(&valid), "{valid}");
        let wrong = format!("ZZ{:02}ABCD1234567890", (check % 97) + 1);
        assert!(!iban(&wrong), "{wrong}");
    }

    #[test]
    fn email_mx_shape_vectors() {
        let cases: &[(&str, bool)] = &[
            ("alex@mail.co.uk", true),
            ("sam.lee+news@shop.io", true),
            ("Alex@Mail.Co", true),
            ("first_last@sub.domain.dev", true),
            ("a@xn--80ak6aa92e.xn--p1ai", true),
            // documentation and test domains never receive mail
            ("alex@example.com", false),
            ("alex@mail.example.org", false),
            ("sam@EXAMPLE.NET", false),
            ("alex@shop.test", false),
            ("alex@host.invalid", false),
            ("alex@printer.local", false),
            ("alex@localhost", false),
            // shape failures
            ("alex", false),
            ("@mail.co", false),
            ("alex@", false),
            ("alex@mail", false),
            ("alex@mail.c", false),
            ("alex@mail.c0m", false),
            ("alex@-mail.co", false),
            ("alex@mail..co", false),
            (".alex@mail.co", false),
            ("al..ex@mail.co", false),
            ("al ex@mail.co", false),
            ("alex@ma il.co", false),
            ("", false),
        ];
        for (text, want) in cases {
            assert_eq!(email_mx_shape(text), *want, "{text:?}");
        }
    }

    #[test]
    fn names_round_trip() {
        for v in [Validator::Luhn, Validator::Iban, Validator::EmailMxShape] {
            assert_eq!(Validator::from_name(v.as_str()), Some(v));
            assert!(Validator::NAMES.contains(v.as_str()));
        }
        assert_eq!(Validator::from_name("LUHN"), None);
    }
}
