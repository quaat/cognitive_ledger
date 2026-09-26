//! Protocol timestamps (ADR-0011): parsed as RFC 3339, normalized to one canonical
//! serialization before hashing, so no client-formatting choice can enter commit identity.

use crate::LedgerError;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

/// A UTC instant at microsecond precision with exactly one serialization:
/// `YYYY-MM-DDTHH:MM:SS.ffffffZ` (27 bytes, always six fractional digits, always `Z`).
///
/// Normalization rules (frozen with commit v2, ADR-0009/0011):
/// - input must be ASCII RFC 3339 with an uppercase `T` separator and, if present, an
///   uppercase `Z`; a numeric offset must have hours 00–23 and minutes 00–59 and is
///   converted to UTC;
/// - fractional seconds may carry 1..=9 digits and are truncated (toward zero) to
///   microseconds *at construction*, so the stored value, its serialization, and
///   equality always agree; sub-microsecond precision never influences identity;
/// - leap seconds (`:60`) are rejected because they have no unique UTC instant;
/// - the UTC year after conversion must lie in 0001..=9999; anything else is rejected
///   (never wrapped, never a panic).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LedgerTimestamp(OffsetDateTime);

impl LedgerTimestamp {
    /// Byte length of the canonical serialization.
    pub const CANONICAL_LEN: usize = 27;

    /// Parse any RFC 3339 date-time and normalize it.
    pub fn parse_rfc3339(input: &str) -> Result<Self, LedgerError> {
        let invalid = |reason: &str| LedgerError::InvalidTimestamp(format!("{input:?}: {reason}"));
        if !input.is_ascii() {
            return Err(invalid("must be ASCII"));
        }
        let bytes = input.as_bytes();
        if bytes.len() < 20 {
            return Err(invalid("too short for an RFC 3339 date-time"));
        }
        if bytes[10] != b'T' {
            return Err(invalid(
                "date and time must be separated by an uppercase 'T'",
            ));
        }
        if bytes[bytes.len() - 1] == b'z' {
            return Err(invalid("the UTC designator must be an uppercase 'Z'"));
        }
        if &bytes[17..19] == b"60" {
            return Err(invalid("leap seconds are not representable"));
        }
        if let Some(fraction) = input[19..].strip_prefix('.') {
            let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 || digits > 9 {
                return Err(invalid("fractional seconds must have 1 to 9 digits"));
            }
        }
        if bytes[bytes.len() - 1] != b'Z' {
            // `±HH:MM`: bound the offset explicitly rather than trusting the parser.
            if bytes.len() < 6 || bytes[bytes.len() - 3] != b':' {
                return Err(invalid("offset must be 'Z' or ±HH:MM"));
            }
            let hours = &input[bytes.len() - 5..bytes.len() - 3];
            let minutes = &input[bytes.len() - 2..];
            let in_range = |part: &str, max: u8| {
                part.len() == 2
                    && part.bytes().all(|b| b.is_ascii_digit())
                    && part.parse::<u8>().is_ok_and(|value| value <= max)
            };
            if !in_range(hours, 23) || !in_range(minutes, 59) {
                return Err(invalid("offset hours must be 00-23 and minutes 00-59"));
            }
        }
        let parsed = OffsetDateTime::parse(input, &Rfc3339).map_err(|e| invalid(&e.to_string()))?;
        Self::try_from_offset_date_time(parsed).map_err(|e| invalid(&e.to_string()))
    }

    /// Normalize an already-parsed instant (for example the service layer's clock when it
    /// assigns `recorded_at`): convert to UTC, truncate to microseconds, and enforce the
    /// year range. Core never reads a clock itself.
    pub fn try_from_offset_date_time(value: OffsetDateTime) -> Result<Self, LedgerError> {
        let utc = value.checked_to_offset(UtcOffset::UTC).ok_or_else(|| {
            LedgerError::InvalidTimestamp("instant is outside the supported range".into())
        })?;
        if !(1..=9999).contains(&utc.year()) {
            return Err(LedgerError::InvalidTimestamp(format!(
                "UTC year {} is outside 0001..=9999",
                utc.year()
            )));
        }
        let truncated = utc
            .replace_nanosecond(utc.nanosecond() / 1_000 * 1_000)
            .map_err(|e| LedgerError::InvalidTimestamp(e.to_string()))?;
        Ok(Self(truncated))
    }

    /// Parse a string that MUST already be in canonical form (used by the commit decoder
    /// so stored bytes can never carry a non-canonical timestamp).
    pub fn parse_canonical(input: &str) -> Result<Self, LedgerError> {
        let parsed = Self::parse_rfc3339(input)?;
        if parsed.canonical() != input {
            return Err(LedgerError::InvalidTimestamp(format!(
                "{input:?} is not in canonical form {:?}",
                parsed.canonical()
            )));
        }
        Ok(parsed)
    }

    /// The single canonical serialization.
    pub fn canonical(&self) -> String {
        let t = self.0;
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:06}Z",
            t.year(),
            u8::from(t.month()),
            t.day(),
            t.hour(),
            t.minute(),
            t.second(),
            t.microsecond()
        )
    }
}

impl fmt::Display for LedgerTimestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

impl Serialize for LedgerTimestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.canonical())
    }
}

impl<'de> Deserialize<'de> for LedgerTimestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Self::parse_rfc3339(&raw).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_offsets_and_precision_to_one_form() {
        let cases = [
            ("2026-09-24T10:00:00Z", "2026-09-24T10:00:00.000000Z"),
            ("2026-09-24T12:00:00+02:00", "2026-09-24T10:00:00.000000Z"),
            ("2026-09-24T09:30:00-00:30", "2026-09-24T10:00:00.000000Z"),
            ("2026-09-24T10:00:00.5Z", "2026-09-24T10:00:00.500000Z"),
            (
                "2026-09-24T10:00:00.1234567Z",
                "2026-09-24T10:00:00.123456Z",
            ),
            (
                "2026-09-24T10:00:00.999999999Z",
                "2026-09-24T10:00:00.999999Z",
            ),
            ("2026-12-31T23:30:00-01:00", "2027-01-01T00:30:00.000000Z"),
            ("0001-01-01T00:00:00Z", "0001-01-01T00:00:00.000000Z"),
            (
                "9999-12-31T23:59:59.9999999Z",
                "9999-12-31T23:59:59.999999Z",
            ),
        ];
        for (input, expected) in cases {
            let parsed = LedgerTimestamp::parse_rfc3339(input).unwrap();
            assert_eq!(parsed.canonical(), expected, "input {input}");
            assert_eq!(parsed.canonical().len(), LedgerTimestamp::CANONICAL_LEN);
            assert_eq!(LedgerTimestamp::parse_canonical(expected).unwrap(), parsed);
        }
    }

    #[test]
    fn sub_microsecond_precision_never_reaches_the_value() {
        let fine = LedgerTimestamp::parse_rfc3339("2026-09-24T10:00:00.1234567Z").unwrap();
        let coarse = LedgerTimestamp::parse_rfc3339("2026-09-24T10:00:00.123456Z").unwrap();
        assert_eq!(fine, coarse);
        let nanos = OffsetDateTime::parse("2026-09-24T10:00:00.999999999Z", &Rfc3339).unwrap();
        let normalized = LedgerTimestamp::try_from_offset_date_time(nanos).unwrap();
        assert_eq!(
            LedgerTimestamp::parse_canonical(&normalized.canonical()).unwrap(),
            normalized
        );
    }

    #[test]
    fn year_and_offset_bounds_are_enforced_not_wrapped() {
        for bad in [
            "0000-01-01T00:00:00Z",
            "0000-01-01T00:30:00+01:00",
            "0001-01-01T00:00:00+00:01",
            "9999-12-31T23:30:00-01:00",
            "2026-09-24T10:00:00+00:60",
            "2026-09-24T10:00:00+24:00",
            "2026-09-24T10:00:00+2:00",
        ] {
            assert!(
                LedgerTimestamp::parse_rfc3339(bad).is_err(),
                "accepted {bad:?}"
            );
        }
        assert!(LedgerTimestamp::parse_rfc3339("2026-09-24T10:00:00+23:59").is_ok());
        assert!(LedgerTimestamp::parse_rfc3339("2026-09-24T10:00:00-00:00").is_ok());
    }

    #[test]
    fn rejects_ambiguous_or_malformed_input() {
        for bad in [
            "",
            "2026-09-24",
            "2026-09-24 10:00:00Z",
            "2026-09-24t10:00:00Z",
            "2026-09-24T10:00:00z",
            "2026-09-24T10:00:00",
            "2026-09-24T10:00:60Z",
            "2026-09-24T10:00:00.Z",
            "2026-09-24T10:00:00.1234567890Z",
            "2026-13-01T00:00:00Z",
            "2026-09-24T10:00:00Ｚ",
            "not a time",
        ] {
            assert!(
                LedgerTimestamp::parse_rfc3339(bad).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    #[test]
    fn canonical_parser_rejects_non_canonical_forms() {
        assert!(LedgerTimestamp::parse_canonical("2026-09-24T10:00:00Z").is_err());
        assert!(LedgerTimestamp::parse_canonical("2026-09-24T12:00:00.000000+02:00").is_err());
        assert!(LedgerTimestamp::parse_canonical("2026-09-24T10:00:00.000000Z").is_ok());
    }

    #[test]
    fn serde_round_trips_through_the_canonical_string() {
        let ts: LedgerTimestamp = serde_json::from_str("\"2026-09-24T12:00:00+02:00\"").unwrap();
        assert_eq!(
            serde_json::to_string(&ts).unwrap(),
            "\"2026-09-24T10:00:00.000000Z\""
        );
        assert!(serde_json::from_str::<LedgerTimestamp>("\"2026-09-24\"").is_err());
    }
}
