//! `sculpin-semantic-context/v1` (ADR-0018): the reproducibility contract naming exactly
//! which candidate state was validated against which base KB, ontology, shape set,
//! reasoning configuration, external (Virtual A-Box) sources and validator. Every
//! semantic identifier is opaque to the ledger.
//!
//! ```text
//! "sculpin-semantic-context-v1\0"
//! field graph_id · field candidate_commit · field candidate_state_digest
//! field base_kb.kb_id · field base_kb.revision
//! u8    ontology tag (0x00 | 0x01 field id, field version)
//! field shapes.id · field shapes.version
//! field reasoning.profile · field reasoning.implementation · field reasoning.version
//! u32   virtual_context_count (<= 64), elements bytewise ascending + unique:
//!         field dataset_id · field source_version
//!         u32 object_ref_count (<= 64) field × n (bytewise ascending, unique)
//!         field query_spec_digest · field hydration_plan_digest
//! field validator.service_id · field validator.service_version
//! field validator.configuration_version
//! ```

use crate::{
    ProtocolError,
    encoding::{Cursor, TAG_ABSENT, TAG_PRESENT, canonical_set, field, u32be},
    typed_id,
};
use ledger_core::{CommitId, ContentId, GraphId, MAX_IDENTIFIER_BYTES, validate_token};
use serde::{Deserialize, Deserializer, Serialize, de::Error as _};

pub const SEMANTIC_CONTEXT_V1_HEADER: &[u8] = b"sculpin-semantic-context-v1\0";
/// Maximum number of distinct Virtual A-Box references in one context.
pub const MAX_VIRTUAL_CONTEXTS: usize = 64;
/// Maximum number of distinct object/version references per Virtual A-Box reference.
pub const MAX_OBJECT_REFS: usize = 64;

typed_id!(
    /// Content identity of a canonical `SemanticExecutionContext`.
    SemanticContextId
);

fn token(field_name: &'static str, value: &str) -> Result<(), ProtocolError> {
    validate_token(field_name, value, MAX_IDENTIFIER_BYTES).map_err(ProtocolError::from)
}

/// The base semantic state Sculpin validated against. `revision` is an opaque stable
/// identifier Sculpin composes (for example from `source_graph_hash`, `shapes_hash` and an
/// ontology version); the ledger never derives or interprets it.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseKb {
    pub kb_id: String,
    pub revision: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ontology {
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShapeSet {
    pub id: String,
    pub version: String,
}

/// Which reasoning ran. A validator that reasons not at all names that as a token of its
/// own choosing (the ledger does not interpret `profile`).
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reasoning {
    pub profile: String,
    pub implementation: String,
    pub version: String,
}

/// Who validated. `service_id` is deployment configuration of the ledger (the identity of
/// the validation service it is configured to call), never client input; the versions are
/// declared by the validator in its response.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidatorIdentity {
    pub service_id: String,
    pub service_version: String,
    pub configuration_version: String,
}

impl ValidatorIdentity {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        token("validator.service_id", &self.service_id)?;
        token("validator.service_version", &self.service_version)?;
        token(
            "validator.configuration_version",
            &self.configuration_version,
        )
    }
}

/// Identifying provenance of transient external state (Virtual A-Box) a validation
/// consulted: never the triples themselves. `object_refs` is a set.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VirtualContextRef {
    pub dataset_id: String,
    pub source_version: String,
    #[serde(default)]
    pub object_refs: Vec<String>,
    pub query_spec_digest: ContentId,
    pub hydration_plan_digest: ContentId,
}

impl VirtualContextRef {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        token("virtual_context.dataset_id", &self.dataset_id)?;
        token("virtual_context.source_version", &self.source_version)?;
        for reference in &self.object_refs {
            token("virtual_context.object_ref", reference)?;
        }
        if self.canonical_object_refs().len() > MAX_OBJECT_REFS {
            return Err(ProtocolError::Invalid(format!(
                "more than {MAX_OBJECT_REFS} distinct object references in a virtual context"
            )));
        }
        Ok(())
    }

    pub fn canonical_object_refs(&self) -> Vec<String> {
        let mut refs = self.object_refs.clone();
        refs.sort_unstable();
        refs.dedup();
        refs
    }

    /// The element encoding (also the sort key inside a context).
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        let mut out = Vec::new();
        field(&mut out, &self.dataset_id)?;
        field(&mut out, &self.source_version)?;
        let refs = self.canonical_object_refs();
        u32be(&mut out, refs.len())?;
        for reference in &refs {
            field(&mut out, reference)?;
        }
        field(&mut out, &self.query_spec_digest.to_string())?;
        field(&mut out, &self.hydration_plan_digest.to_string())?;
        Ok(out)
    }

    pub(crate) fn decode(cursor: &mut Cursor<'_>) -> Result<Self, ProtocolError> {
        let dataset_id = cursor.field("dataset_id")?;
        let source_version = cursor.field("source_version")?;
        let object_refs = cursor.set(
            "object references",
            MAX_OBJECT_REFS,
            |c| c.field("object_ref"),
            |s| {
                let mut out = Vec::new();
                field(&mut out, s)?;
                Ok(out)
            },
        )?;
        let query_spec_digest: ContentId = cursor.field("query_spec_digest")?.parse()?;
        let hydration_plan_digest: ContentId = cursor.field("hydration_plan_digest")?.parse()?;
        let value = Self {
            dataset_id,
            source_version,
            object_refs,
            query_spec_digest,
            hydration_plan_digest,
        };
        value.validate()?;
        Ok(value)
    }
}

/// `Eq` is structural (as given, including caller order of the sets); identity is `id()`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SemanticExecutionContext {
    pub graph_id: GraphId,
    pub candidate_commit: CommitId,
    /// `sculpin-rdf-state/v1` digest of the reconstructed candidate dataset.
    pub candidate_state_digest: ContentId,
    pub base_kb: BaseKb,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ontology: Option<Ontology>,
    pub shapes: ShapeSet,
    pub reasoning: Reasoning,
    /// A set: canonicalized sorted + unique.
    pub virtual_contexts: Vec<VirtualContextRef>,
    pub validator: ValidatorIdentity,
}

impl SemanticExecutionContext {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        token("base_kb.kb_id", &self.base_kb.kb_id)?;
        token("base_kb.revision", &self.base_kb.revision)?;
        if let Some(ontology) = &self.ontology {
            token("ontology.id", &ontology.id)?;
            token("ontology.version", &ontology.version)?;
        }
        token("shapes.id", &self.shapes.id)?;
        token("shapes.version", &self.shapes.version)?;
        token("reasoning.profile", &self.reasoning.profile)?;
        token("reasoning.implementation", &self.reasoning.implementation)?;
        token("reasoning.version", &self.reasoning.version)?;
        for context in &self.virtual_contexts {
            context.validate()?;
        }
        if self.canonical_virtual_contexts()?.len() > MAX_VIRTUAL_CONTEXTS {
            return Err(ProtocolError::Invalid(format!(
                "more than {MAX_VIRTUAL_CONTEXTS} distinct virtual contexts"
            )));
        }
        self.validator.validate()
    }

    /// The virtual-context set as it enters canonical bytes: each element encoded, sorted
    /// bytewise, unique.
    pub fn canonical_virtual_contexts(&self) -> Result<Vec<Vec<u8>>, ProtocolError> {
        let encoded = self
            .virtual_contexts
            .iter()
            .map(VirtualContextRef::canonical_bytes)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(canonical_set(encoded))
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ProtocolError> {
        self.validate()?;
        let mut out = SEMANTIC_CONTEXT_V1_HEADER.to_vec();
        field(&mut out, self.graph_id.as_str())?;
        field(&mut out, &self.candidate_commit.to_string())?;
        field(&mut out, &self.candidate_state_digest.to_string())?;
        field(&mut out, &self.base_kb.kb_id)?;
        field(&mut out, &self.base_kb.revision)?;
        match &self.ontology {
            None => out.push(TAG_ABSENT),
            Some(ontology) => {
                out.push(TAG_PRESENT);
                field(&mut out, &ontology.id)?;
                field(&mut out, &ontology.version)?;
            }
        }
        field(&mut out, &self.shapes.id)?;
        field(&mut out, &self.shapes.version)?;
        field(&mut out, &self.reasoning.profile)?;
        field(&mut out, &self.reasoning.implementation)?;
        field(&mut out, &self.reasoning.version)?;
        let contexts = self.canonical_virtual_contexts()?;
        u32be(&mut out, contexts.len())?;
        for element in &contexts {
            out.extend_from_slice(element);
        }
        field(&mut out, &self.validator.service_id)?;
        field(&mut out, &self.validator.service_version)?;
        field(&mut out, &self.validator.configuration_version)?;
        Ok(out)
    }

    pub fn id(&self) -> Result<SemanticContextId, ProtocolError> {
        Ok(SemanticContextId(ContentId::for_bytes(
            &self.canonical_bytes()?,
        )))
    }

    /// Strict decoder: yields the canonical (sorted, unique) form or rejects.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let mut cursor = Cursor::new(bytes, SEMANTIC_CONTEXT_V1_HEADER, "semantic context")?;
        let graph_id = GraphId::new(cursor.field("graph_id")?)?;
        let candidate_commit: CommitId = cursor.field("candidate_commit")?.parse()?;
        let candidate_state_digest: ContentId = cursor.field("candidate_state_digest")?.parse()?;
        let base_kb = BaseKb {
            kb_id: cursor.field("base_kb.kb_id")?,
            revision: cursor.field("base_kb.revision")?,
        };
        let ontology = match cursor.u8("ontology tag")? {
            TAG_ABSENT => None,
            TAG_PRESENT => Some(Ontology {
                id: cursor.field("ontology.id")?,
                version: cursor.field("ontology.version")?,
            }),
            other => {
                return Err(ProtocolError::Invalid(format!(
                    "semantic context: unknown ontology tag {other}"
                )));
            }
        };
        let shapes = ShapeSet {
            id: cursor.field("shapes.id")?,
            version: cursor.field("shapes.version")?,
        };
        let reasoning = Reasoning {
            profile: cursor.field("reasoning.profile")?,
            implementation: cursor.field("reasoning.implementation")?,
            version: cursor.field("reasoning.version")?,
        };
        let virtual_contexts = cursor.set(
            "virtual contexts",
            MAX_VIRTUAL_CONTEXTS,
            VirtualContextRef::decode,
            VirtualContextRef::canonical_bytes,
        )?;
        let validator = ValidatorIdentity {
            service_id: cursor.field("validator.service_id")?,
            service_version: cursor.field("validator.service_version")?,
            configuration_version: cursor.field("validator.configuration_version")?,
        };
        cursor.finish()?;
        let context = Self {
            graph_id,
            candidate_commit,
            candidate_state_digest,
            base_kb,
            ontology,
            shapes,
            reasoning,
            virtual_contexts,
            validator,
        };
        context.validate()?;
        Ok(context)
    }
}

impl<'de> Deserialize<'de> for SemanticExecutionContext {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            graph_id: GraphId,
            candidate_commit: CommitId,
            candidate_state_digest: ContentId,
            base_kb: BaseKb,
            #[serde(default)]
            ontology: Option<Ontology>,
            shapes: ShapeSet,
            reasoning: Reasoning,
            #[serde(default)]
            virtual_contexts: Vec<VirtualContextRef>,
            validator: ValidatorIdentity,
        }
        let wire = Wire::deserialize(deserializer)?;
        let context = Self {
            graph_id: wire.graph_id,
            candidate_commit: wire.candidate_commit,
            candidate_state_digest: wire.candidate_state_digest,
            base_kb: wire.base_kb,
            ontology: wire.ontology,
            shapes: wire.shapes,
            reasoning: wire.reasoning,
            virtual_contexts: wire.virtual_contexts,
            validator: wire.validator,
        };
        context.validate().map_err(D::Error::custom)?;
        Ok(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(s: &str) -> ContentId {
        ContentId::for_bytes(s.as_bytes())
    }

    fn vc(dataset: &str, version: &str, refs: &[&str]) -> VirtualContextRef {
        VirtualContextRef {
            dataset_id: dataset.into(),
            source_version: version.into(),
            object_refs: refs.iter().map(|r| (*r).to_owned()).collect(),
            query_spec_digest: digest("q"),
            hydration_plan_digest: digest("h"),
        }
    }

    pub(crate) fn sample() -> SemanticExecutionContext {
        SemanticExecutionContext {
            graph_id: GraphId::new("graph-1").unwrap(),
            candidate_commit: CommitId(digest("candidate")),
            candidate_state_digest: digest("state"),
            base_kb: BaseKb {
                kb_id: "urn:exodus:kb:42".into(),
                revision: "kbrev-7".into(),
            },
            ontology: Some(Ontology {
                id: "urn:sculpin:ontology:core".into(),
                version: "3.1.0".into(),
            }),
            shapes: ShapeSet {
                id: "urn:sculpin:shapes:core".into(),
                version: "12".into(),
            },
            reasoning: Reasoning {
                profile: "rdfs".into(),
                implementation: "sculpin-python-reasoner".into(),
                version: "0.9.2".into(),
            },
            virtual_contexts: vec![vc("ds-b", "v1", &["o2", "o1"]), vc("ds-a", "v1", &["o1"])],
            validator: ValidatorIdentity {
                service_id: "urn:sculpin:service:validator".into(),
                service_version: "2026.09".into(),
                configuration_version: "cfg-5".into(),
            },
        }
    }

    #[test]
    fn round_trips_and_normalizes_sets() {
        let context = sample();
        let bytes = context.canonical_bytes().unwrap();
        let decoded = SemanticExecutionContext::from_canonical_bytes(&bytes).unwrap();
        assert_eq!(decoded.canonical_bytes().unwrap(), bytes);
        assert_eq!(decoded.id().unwrap(), context.id().unwrap());
        assert_eq!(decoded.virtual_contexts[0].dataset_id, "ds-a");
        assert_eq!(decoded.virtual_contexts[1].object_refs, vec!["o1", "o2"]);
        let mut duplicated = context.clone();
        duplicated
            .virtual_contexts
            .push(vc("ds-a", "v1", &["o1", "o1"]));
        assert_eq!(duplicated.id().unwrap(), context.id().unwrap());
    }

    #[test]
    fn external_source_version_changes_identity() {
        let a = sample();
        let mut b = sample();
        b.virtual_contexts[1] = vc("ds-a", "v2", &["o1"]);
        assert_ne!(a.id().unwrap(), b.id().unwrap());
        let mut c = sample();
        c.ontology = None;
        assert_ne!(a.id().unwrap(), c.id().unwrap());
        let c_bytes = c.canonical_bytes().unwrap();
        let decoded = SemanticExecutionContext::from_canonical_bytes(&c_bytes).unwrap();
        assert_eq!(decoded.canonical_bytes().unwrap(), c_bytes);
        assert_eq!(decoded.ontology, None);
    }

    #[test]
    fn strictness_and_bounds() {
        let context = sample();
        let mut trailing = context.canonical_bytes().unwrap();
        trailing.push(0);
        assert!(SemanticExecutionContext::from_canonical_bytes(&trailing).is_err());
        let mut empty = sample();
        empty.base_kb.revision = String::new();
        assert!(empty.canonical_bytes().is_err());
        let mut too_many = sample();
        too_many.virtual_contexts = (0..=MAX_VIRTUAL_CONTEXTS)
            .map(|i| vc(&format!("ds-{i:03}"), "v", &[]))
            .collect();
        assert!(too_many.canonical_bytes().is_err());
        let mut control = sample();
        control.shapes.version = "1\n2".into();
        assert!(control.canonical_bytes().is_err());
        // Ontology present-but-empty on the wire is refused.
        let mut ontology_empty = sample();
        ontology_empty.ontology = Some(Ontology {
            id: String::new(),
            version: "1".into(),
        });
        assert!(ontology_empty.canonical_bytes().is_err());
    }

    #[test]
    fn decoder_rejects_unsorted_virtual_contexts() {
        let context = sample();
        let sorted = context.canonical_virtual_contexts().unwrap();
        let canonical = context.canonical_bytes().unwrap();
        let mut pair = Vec::new();
        for e in &sorted {
            pair.extend_from_slice(e);
        }
        let mut swapped = Vec::new();
        for e in sorted.iter().rev() {
            swapped.extend_from_slice(e);
        }
        let at = canonical
            .windows(pair.len())
            .position(|w| w == pair.as_slice())
            .unwrap();
        let mut bytes = canonical.clone();
        bytes[at..at + pair.len()].copy_from_slice(&swapped);
        assert!(SemanticExecutionContext::from_canonical_bytes(&bytes).is_err());
    }

    #[test]
    fn serde_validates_and_round_trips() {
        let json = serde_json::to_string(&sample()).unwrap();
        let back: SemanticExecutionContext = serde_json::from_str(&json).unwrap();
        assert_eq!(back, sample());
        let unknown = json.replacen("\"graph_id\"", "\"tenant_id\":\"t\",\"graph_id\"", 1);
        assert!(serde_json::from_str::<SemanticExecutionContext>(&unknown).is_err());
        let empty = json.replacen("\"kbrev-7\"", "\"\"", 1);
        assert!(serde_json::from_str::<SemanticExecutionContext>(&empty).is_err());
    }
}
