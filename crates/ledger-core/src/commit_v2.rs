//! `sculpin-cognitive-commit/v2`: the production provenance envelope (ADR-0009).
//!
//! Canonical byte layout (all integers big-endian; `field` = `u32` length + UTF-8 bytes;
//! `opt` = one tag byte `0x00` (absent) or `0x01` followed by a non-empty `field`):
//!
//! ```text
//! "sculpin-cognitive-commit-v2\0"
//! field  graph_id
//! u32    parent_count (0..=2)      field parent  × parent_count   (ordered; distinct)
//! field  patch_id
//! field  principal_id
//! u8     principal_type            (0 human, 1 agent, 2 service)
//! opt    on_behalf_of
//! field  activity
//! opt    event_time                (canonical LedgerTimestamp)
//! field  recorded_at               (canonical LedgerTimestamp; server-assigned)
//! u32    evidence_count (<= 64)    field evidence_ref × evidence_count
//!                                  (strictly ascending bytewise ⇒ sorted + unique)
//! opt    source_system             (caller-declared; informational only)
//! field  message                   (<= 4096 bytes; may be empty)
//! ```
//!
//! Absent and empty are never conflated: every optional field is either the tag `0x00` or
//! a non-empty value, and the decoder rejects `0x01` + empty. `evidence_refs[]` is a set:
//! the encoder sorts and de-duplicates, the decoder rejects anything not strictly
//! ascending, so caller order can never reach `CommitId`. `correlation_id` is deliberately
//! not part of this envelope.

use crate::{
    Actor, CommitId, GraphId, LedgerError, LedgerTimestamp, MAX_EVIDENCE_REFS,
    MAX_IDENTIFIER_BYTES, MAX_MESSAGE_BYTES, PatchId, PrincipalId, PrincipalType, encode_field,
    identity::validate_token, read_field,
};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

/// Header bytes that identify a v2 envelope. Distinct from the v1 `sculpin-commit-v1\0`.
pub const COMMIT_V2_HEADER: &[u8] = b"sculpin-cognitive-commit-v2\0";

const TAG_ABSENT: u8 = 0;
const TAG_PRESENT: u8 = 1;

/// `Eq` is structural (fields as given, including caller order of `evidence_refs`);
/// identity is `id()`. Two values that differ only in evidence order are `!=` yet share a
/// `CommitId`; the decoder always yields the canonical (sorted, unique) order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CommitV2 {
    /// The graph this commit belongs to (ADR-0010). A commit belongs to exactly one graph.
    pub graph_id: GraphId,
    /// Ordered parents; parent zero is the reconstruction parent (ADR-0006).
    pub parents: Vec<CommitId>,
    /// The effective patch (ADR-0008).
    pub patch: PatchId,
    /// Populated only from the authenticated principal (ADR-0011).
    pub actor: Actor,
    /// What kind of cognitive activity produced the change (bounded token).
    pub activity: String,
    /// Client-declared event time, normalized; optional per policy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_time: Option<LedgerTimestamp>,
    /// Server-assigned; identity-bearing, so a `CommitId` is re-verifiable but not
    /// derivable from logical inputs alone.
    pub recorded_at: LedgerTimestamp,
    /// Durable evidence references. Logically a set; canonicalized sorted + unique.
    pub evidence_refs: Vec<String>,
    /// Caller-declared origin. Informational only — never trusted provenance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_system: Option<String>,
    /// Free text, bounded.
    pub message: String,
}

impl CommitV2 {
    /// Structural validation shared by the encoder, the decoder, and serde.
    pub fn validate(&self) -> Result<(), LedgerError> {
        if self.parents.len() > 2 {
            return Err(LedgerError::InvalidCommit(
                "protocol v2 permits at most two parents".into(),
            ));
        }
        if self.parents.len() == 2 && self.parents[0] == self.parents[1] {
            return Err(LedgerError::InvalidCommit(
                "merge parents must be distinct".into(),
            ));
        }
        validate_token("activity", &self.activity, MAX_IDENTIFIER_BYTES)?;
        for evidence in &self.evidence_refs {
            validate_token("evidence_ref", evidence, MAX_IDENTIFIER_BYTES)?;
        }
        if self.canonical_evidence_refs().len() > MAX_EVIDENCE_REFS {
            return Err(LedgerError::InvalidCommit(format!(
                "more than {MAX_EVIDENCE_REFS} distinct evidence references"
            )));
        }
        if let Some(source) = &self.source_system {
            validate_token("source_system", source, MAX_IDENTIFIER_BYTES)?;
        }
        if self.message.len() > MAX_MESSAGE_BYTES {
            return Err(LedgerError::InvalidCommit(format!(
                "message exceeds {MAX_MESSAGE_BYTES} bytes"
            )));
        }
        Ok(())
    }

    /// The evidence set as it enters canonical bytes: sorted bytewise on UTF-8, unique.
    pub fn canonical_evidence_refs(&self) -> Vec<String> {
        let mut refs = self.evidence_refs.clone();
        refs.sort_unstable();
        refs.dedup();
        refs
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, LedgerError> {
        self.validate()?;
        let mut out = COMMIT_V2_HEADER.to_vec();
        encode_field(&mut out, self.graph_id.as_str())?;
        out.extend_from_slice(
            &u32::try_from(self.parents.len())
                .expect("parent count is bounded by two")
                .to_be_bytes(),
        );
        for parent in &self.parents {
            encode_field(&mut out, &parent.to_string())?;
        }
        encode_field(&mut out, &self.patch.to_string())?;
        encode_field(&mut out, self.actor.principal_id.as_str())?;
        out.push(self.actor.principal_type.wire_byte());
        encode_optional(
            &mut out,
            self.actor.on_behalf_of.as_ref().map(PrincipalId::as_str),
        )?;
        encode_field(&mut out, &self.activity)?;
        encode_optional(&mut out, self.event_time.map(|t| t.canonical()).as_deref())?;
        encode_field(&mut out, &self.recorded_at.canonical())?;
        let evidence = self.canonical_evidence_refs();
        out.extend_from_slice(
            &u32::try_from(evidence.len())
                .expect("evidence count is bounded")
                .to_be_bytes(),
        );
        for reference in &evidence {
            encode_field(&mut out, reference)?;
        }
        encode_optional(&mut out, self.source_system.as_deref())?;
        encode_field(&mut out, &self.message)?;
        Ok(out)
    }

    pub fn id(&self) -> Result<CommitId, LedgerError> {
        Ok(CommitId(crate::ContentId::for_bytes(
            &self.canonical_bytes()?,
        )))
    }

    /// Strict decoder: every rule the encoder applies is re-checked, so bytes that are not
    /// the canonical form of some logical commit are rejected rather than reinterpreted.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, LedgerError> {
        let mut rest = bytes
            .strip_prefix(COMMIT_V2_HEADER)
            .ok_or_else(|| LedgerError::InvalidCommit("unknown header".into()))?;
        let graph_id = GraphId::new(read_field(&mut rest)?)?;
        let parent_count = read_u32(&mut rest, "parent count")? as usize;
        if parent_count > 2 {
            return Err(LedgerError::InvalidCommit(
                "protocol v2 permits at most two parents".into(),
            ));
        }
        let mut parents = Vec::with_capacity(parent_count);
        for _ in 0..parent_count {
            parents.push(read_field(&mut rest)?.parse()?);
        }
        let patch: PatchId = read_field(&mut rest)?.parse()?;
        let principal_id = PrincipalId::new(read_field(&mut rest)?)?;
        let principal_type = PrincipalType::from_wire_byte(read_u8(&mut rest, "principal_type")?)?;
        let on_behalf_of = read_optional(&mut rest, "on_behalf_of")?
            .map(PrincipalId::new)
            .transpose()?;
        let activity = read_field(&mut rest)?;
        let event_time = read_optional(&mut rest, "event_time")?
            .map(|raw| LedgerTimestamp::parse_canonical(&raw))
            .transpose()?;
        let recorded_at = LedgerTimestamp::parse_canonical(&read_field(&mut rest)?)?;
        let evidence_count = read_u32(&mut rest, "evidence count")? as usize;
        if evidence_count > MAX_EVIDENCE_REFS {
            return Err(LedgerError::InvalidCommit(format!(
                "more than {MAX_EVIDENCE_REFS} evidence references"
            )));
        }
        let mut evidence_refs: Vec<String> = Vec::with_capacity(evidence_count);
        for _ in 0..evidence_count {
            let reference = read_field(&mut rest)?;
            if let Some(previous) = evidence_refs.last()
                && previous.as_bytes() >= reference.as_bytes()
            {
                return Err(LedgerError::InvalidCommit(
                    "evidence references must be strictly ascending (sorted, unique)".into(),
                ));
            }
            evidence_refs.push(reference);
        }
        let source_system = read_optional(&mut rest, "source_system")?;
        let message = read_field(&mut rest)?;
        if !rest.is_empty() {
            return Err(LedgerError::InvalidCommit("trailing bytes".into()));
        }
        let commit = Self {
            graph_id,
            parents,
            patch,
            actor: Actor {
                principal_id,
                principal_type,
                on_behalf_of,
            },
            activity,
            event_time,
            recorded_at,
            evidence_refs,
            source_system,
            message,
        };
        commit.validate()?;
        Ok(commit)
    }
}

impl<'de> Deserialize<'de> for CommitV2 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            graph_id: GraphId,
            parents: Vec<CommitId>,
            patch: PatchId,
            actor: Actor,
            activity: String,
            #[serde(default)]
            event_time: Option<LedgerTimestamp>,
            recorded_at: LedgerTimestamp,
            #[serde(default)]
            evidence_refs: Vec<String>,
            #[serde(default)]
            source_system: Option<String>,
            message: String,
        }
        let wire = Wire::deserialize(deserializer)?;
        let commit = Self {
            graph_id: wire.graph_id,
            parents: wire.parents,
            patch: wire.patch,
            actor: wire.actor,
            activity: wire.activity,
            event_time: wire.event_time,
            recorded_at: wire.recorded_at,
            evidence_refs: wire.evidence_refs,
            source_system: wire.source_system,
            message: wire.message,
        };
        commit.validate().map_err(D::Error::custom)?;
        Ok(commit)
    }
}

/// Any readable commit envelope. v1 stays readable forever; unknown versions fail closed
/// (ADR-0009 dual read).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AnyCommit {
    V1(crate::Commit),
    V2(CommitV2),
}

impl AnyCommit {
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, LedgerError> {
        if bytes.starts_with(crate::COMMIT_HEADER) {
            return crate::Commit::from_canonical_bytes(bytes).map(Self::V1);
        }
        if bytes.starts_with(COMMIT_V2_HEADER) {
            return CommitV2::from_canonical_bytes(bytes).map(Self::V2);
        }
        let shown: String = bytes
            .iter()
            .take(32)
            .take_while(|b| **b != 0)
            .map(|b| char::from(*b))
            .filter(|c| c.is_ascii_graphic())
            .collect();
        Err(LedgerError::UnknownCommitVersion(shown))
    }

    /// Protocol version tag as stored in a commit index (`1` or `2`).
    pub const fn version(&self) -> u8 {
        match self {
            Self::V1(_) => 1,
            Self::V2(_) => 2,
        }
    }

    /// The graph a commit is bound to by its own bytes. v1 envelopes carry none; their
    /// graph binding is a deployment policy decision (ADR-0010), not commit content.
    pub fn graph_id(&self) -> Option<&GraphId> {
        match self {
            Self::V1(_) => None,
            Self::V2(c) => Some(&c.graph_id),
        }
    }

    pub fn parents(&self) -> &[CommitId] {
        match self {
            Self::V1(c) => &c.parents,
            Self::V2(c) => &c.parents,
        }
    }

    pub fn patch(&self) -> &PatchId {
        match self {
            Self::V1(c) => &c.patch,
            Self::V2(c) => &c.patch,
        }
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, LedgerError> {
        match self {
            Self::V1(c) => c.canonical_bytes(),
            Self::V2(c) => c.canonical_bytes(),
        }
    }

    pub fn id(&self) -> Result<CommitId, LedgerError> {
        match self {
            Self::V1(c) => c.id(),
            Self::V2(c) => c.id(),
        }
    }
}

fn encode_optional(out: &mut Vec<u8>, value: Option<&str>) -> Result<(), LedgerError> {
    match value {
        None => {
            out.push(TAG_ABSENT);
            Ok(())
        }
        Some(present) => {
            if present.is_empty() {
                return Err(LedgerError::InvalidCommit(
                    "optional fields are absent or non-empty, never empty".into(),
                ));
            }
            out.push(TAG_PRESENT);
            encode_field(out, present)
        }
    }
}

fn read_optional(rest: &mut &[u8], field: &str) -> Result<Option<String>, LedgerError> {
    match read_u8(rest, field)? {
        TAG_ABSENT => Ok(None),
        TAG_PRESENT => {
            let value = read_field(rest)?;
            if value.is_empty() {
                return Err(LedgerError::InvalidCommit(format!(
                    "{field}: present-but-empty is not a valid encoding"
                )));
            }
            Ok(Some(value))
        }
        other => Err(LedgerError::InvalidCommit(format!(
            "{field}: unknown optional tag {other}"
        ))),
    }
}

fn read_u8(rest: &mut &[u8], field: &str) -> Result<u8, LedgerError> {
    let Some((first, tail)) = rest.split_first() else {
        return Err(LedgerError::InvalidCommit(format!("truncated {field}")));
    };
    *rest = tail;
    Ok(*first)
}

fn read_u32(rest: &mut &[u8], field: &str) -> Result<u32, LedgerError> {
    if rest.len() < 4 {
        return Err(LedgerError::InvalidCommit(format!("truncated {field}")));
    }
    let value = u32::from_be_bytes(rest[..4].try_into().expect("four bytes"));
    *rest = &rest[4..];
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContentId;

    fn sample() -> CommitV2 {
        CommitV2 {
            graph_id: GraphId::new("graph-1").unwrap(),
            parents: vec![CommitId(ContentId::for_bytes(b"parent"))],
            patch: PatchId(ContentId::for_bytes(b"patch")),
            actor: Actor {
                principal_id: PrincipalId::new("urn:sculpin:agent:curator").unwrap(),
                principal_type: PrincipalType::Agent,
                on_behalf_of: Some(PrincipalId::new("urn:sculpin:human:reviewer").unwrap()),
            },
            activity: "cognitive-correction".into(),
            event_time: Some(LedgerTimestamp::parse_rfc3339("2026-09-24T12:00:00+02:00").unwrap()),
            recorded_at: LedgerTimestamp::parse_rfc3339("2026-09-24T10:01:00Z").unwrap(),
            // Canonical order, so a decoded commit compares equal to the sample.
            evidence_refs: vec!["urn:e:1".into(), "urn:e:2".into()],
            source_system: Some("sculpin-agent-api".into()),
            message: "sample".into(),
        }
    }

    #[test]
    fn round_trips_and_is_stable() {
        let commit = sample();
        let bytes = commit.canonical_bytes().unwrap();
        let decoded = CommitV2::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
        assert_eq!(decoded.evidence_refs, vec!["urn:e:1", "urn:e:2"]);
        assert_eq!(decoded.id().unwrap(), commit.id().unwrap());
        assert_eq!(decoded.event_time, commit.event_time);
    }

    #[test]
    fn evidence_order_and_duplicates_never_change_identity() {
        let a = sample();
        let mut b = sample();
        b.evidence_refs = vec!["urn:e:2".into(), "urn:e:1".into(), "urn:e:2".into()];
        assert_ne!(a, b, "structural equality sees caller order");
        assert_eq!(a.canonical_bytes().unwrap(), b.canonical_bytes().unwrap());
        assert_eq!(
            CommitV2::from_canonical_bytes(&b.canonical_bytes().unwrap()).unwrap(),
            a,
            "decoding always yields the canonical order"
        );
        let mut c = sample();
        c.evidence_refs = vec!["urn:e:1".into()];
        assert_ne!(a.id().unwrap(), c.id().unwrap());
    }

    #[test]
    fn parent_order_is_identity_bearing() {
        let p1 = CommitId(ContentId::for_bytes(b"p1"));
        let p2 = CommitId(ContentId::for_bytes(b"p2"));
        let mut merge = sample();
        merge.parents = vec![p1.clone(), p2.clone()];
        let mut reversed = sample();
        reversed.parents = vec![p2, p1];
        assert_ne!(merge.id().unwrap(), reversed.id().unwrap());
    }

    #[test]
    fn absent_and_empty_optionals_are_distinct_and_empty_is_rejected() {
        let mut absent = sample();
        absent.actor.on_behalf_of = None;
        absent.event_time = None;
        absent.source_system = None;
        let bytes = absent.canonical_bytes().unwrap();
        assert_eq!(CommitV2::from_canonical_bytes(&bytes).unwrap(), absent);
        assert_ne!(bytes, sample().canonical_bytes().unwrap());

        let mut empty_source = sample();
        empty_source.source_system = Some(String::new());
        assert!(empty_source.canonical_bytes().is_err());

        // Forge a present-but-empty source_system on the wire and confirm rejection.
        let mut forged = absent.canonical_bytes().unwrap();
        let message_len = absent.message.len();
        let tail = 4 + message_len; // u32 length + message bytes
        let tag_index = forged.len() - tail - 1;
        assert_eq!(forged[tag_index], TAG_ABSENT);
        forged[tag_index] = TAG_PRESENT;
        let mut with_empty_value = forged[..=tag_index].to_vec();
        with_empty_value.extend_from_slice(&0u32.to_be_bytes());
        with_empty_value.extend_from_slice(&forged[tag_index + 1..]);
        assert!(CommitV2::from_canonical_bytes(&with_empty_value).is_err());
    }

    #[test]
    fn decoder_rejects_unsorted_evidence_bad_type_and_trailing_bytes() {
        let sorted = sample();
        let canonical = sorted.canonical_bytes().unwrap();
        // Build unsorted bytes by hand: swap the two evidence fields.
        let mut sorted_pair = Vec::new();
        encode_field(&mut sorted_pair, "urn:e:1").unwrap();
        encode_field(&mut sorted_pair, "urn:e:2").unwrap();
        let mut unsorted_pair = Vec::new();
        encode_field(&mut unsorted_pair, "urn:e:2").unwrap();
        encode_field(&mut unsorted_pair, "urn:e:1").unwrap();
        let position = canonical
            .windows(sorted_pair.len())
            .position(|w| w == sorted_pair.as_slice())
            .unwrap();
        let mut unsorted = canonical.clone();
        unsorted[position..position + sorted_pair.len()].copy_from_slice(&unsorted_pair);
        assert!(CommitV2::from_canonical_bytes(&unsorted).is_err());

        let mut trailing = canonical.clone();
        trailing.push(0);
        assert!(CommitV2::from_canonical_bytes(&trailing).is_err());

        let type_index = COMMIT_V2_HEADER.len()
            + 4
            + sorted.graph_id.as_str().len()
            + 4
            + (4 + sorted.parents[0].to_string().len())
            + 4
            + sorted.patch.to_string().len()
            + 4
            + sorted.actor.principal_id.as_str().len();
        assert_eq!(canonical[type_index], PrincipalType::Agent.wire_byte());
        let mut bad_type = canonical;
        bad_type[type_index] = 9;
        assert!(CommitV2::from_canonical_bytes(&bad_type).is_err());
    }

    #[test]
    fn caps_are_enforced_on_encode_and_decode() {
        let mut long_message = sample();
        long_message.message = "m".repeat(MAX_MESSAGE_BYTES + 1);
        assert!(long_message.canonical_bytes().is_err());
        let mut many_refs = sample();
        many_refs.evidence_refs = (0..=MAX_EVIDENCE_REFS)
            .map(|i| format!("urn:e:{i:03}"))
            .collect();
        assert!(many_refs.canonical_bytes().is_err());
        let mut dup_heavy = sample();
        dup_heavy.evidence_refs = vec!["urn:e:1".into(); MAX_EVIDENCE_REFS + 5];
        assert!(
            dup_heavy.canonical_bytes().is_ok(),
            "duplicates collapse before the cap"
        );
        let mut bad_activity = sample();
        bad_activity.activity = String::new();
        assert!(bad_activity.canonical_bytes().is_err());
    }

    #[test]
    fn serde_normalizes_and_validates() {
        let json = serde_json::to_string(&sample()).unwrap();
        let back: CommitV2 = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample());
        assert!(json.contains("2026-09-24T10:00:00.000000Z"));
        let unknown_field = json.replacen("\"message\"", "\"correlation_id\":\"x\",\"message\"", 1);
        assert!(serde_json::from_str::<CommitV2>(&unknown_field).is_err());
    }

    #[test]
    fn dual_read_dispatches_on_header_and_fails_closed() {
        let v2 = sample();
        let v2_bytes = v2.canonical_bytes().unwrap();
        assert_eq!(
            AnyCommit::from_canonical_bytes(&v2_bytes).unwrap(),
            AnyCommit::V2(v2.clone())
        );
        let v1 = crate::Commit {
            parents: vec![],
            patch: PatchId(ContentId::for_bytes(b"p")),
            author: "a".into(),
            message: "m".into(),
            event_time: "e".into(),
            recorded_time: "r".into(),
        };
        let any = AnyCommit::from_canonical_bytes(&v1.canonical_bytes().unwrap()).unwrap();
        assert_eq!(any, AnyCommit::V1(v1.clone()));
        assert_eq!(any.id().unwrap(), v1.id().unwrap());
        assert_eq!(any.patch(), &v1.patch);
        assert!(matches!(
            AnyCommit::from_canonical_bytes(b"sculpin-cognitive-commit-v3\0rest"),
            Err(LedgerError::UnknownCommitVersion(_))
        ));
        assert!(matches!(
            AnyCommit::from_canonical_bytes(b""),
            Err(LedgerError::UnknownCommitVersion(_))
        ));
    }
}
