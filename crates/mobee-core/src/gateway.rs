use std::fmt;

use serde::{Deserialize, Serialize};

pub const MOBEE_TAG: &str = "mobee";
pub const PROTOCOL_VERSION: &str = "1";

pub const JOB_OFFER_KIND: u16 = 5109;
pub const JOB_RESULT_KIND: u16 = 6109;
pub const JOB_FEEDBACK_KIND: u16 = 7000;
pub const JOB_RECEIPT_KIND: u16 = 3400;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSpec(pub Vec<String>);

impl TagSpec {
    pub fn new<const N: usize>(values: [&str; N]) -> Self {
        Self(values.into_iter().map(str::to_owned).collect())
    }

    pub fn first(&self) -> Option<&str> {
        self.0.first().map(String::as_str)
    }

    pub fn value(&self) -> Option<&str> {
        self.0.get(1).map(String::as_str)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventDraft {
    pub kind: u16,
    pub tags: Vec<TagSpec>,
    pub content: String,
}

impl EventDraft {
    pub fn new(kind: u16, tags: Vec<TagSpec>, content: impl Into<String>) -> Self {
        Self {
            kind,
            tags,
            content: content.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferDraft {
    pub task: String,
    pub output: String,
    pub amount_sats: u64,
    pub deadline_unix: u64,
    pub mint_url: String,
    pub seller_pubkey: Option<String>,
}

impl OfferDraft {
    pub fn new(
        task: impl Into<String>,
        output: impl Into<String>,
        amount_sats: u64,
        deadline_unix: u64,
        mint_url: impl Into<String>,
        seller_pubkey: impl Into<String>,
    ) -> Self {
        Self {
            task: task.into(),
            output: output.into(),
            amount_sats,
            deadline_unix,
            mint_url: mint_url.into(),
            seller_pubkey: Some(seller_pubkey.into()),
        }
    }

    pub fn untargeted(
        task: impl Into<String>,
        output: impl Into<String>,
        amount_sats: u64,
        deadline_unix: u64,
        mint_url: impl Into<String>,
    ) -> Self {
        Self {
            task: task.into(),
            output: output.into(),
            amount_sats,
            deadline_unix,
            mint_url: mint_url.into(),
            seller_pubkey: None,
        }
    }

    pub fn to_event_draft(&self) -> EventDraft {
        let mut tags = vec![
            TagSpec::new(["i", &self.task]),
            TagSpec::new(["output", &self.output]),
            TagSpec::new(["amount", &self.amount_sats.to_string(), "sat"]),
            TagSpec::new(["param", "deadline", &self.deadline_unix.to_string()]),
            TagSpec::new(["mint", &self.mint_url]),
        ];
        if let Some(seller_pubkey) = &self.seller_pubkey {
            tags.push(TagSpec::new(["p", seller_pubkey]));
        }
        tags.push(mobee_tag());
        tags.push(version_tag());

        EventDraft::new(JOB_OFFER_KIND, tags, "")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ParsedOffer {
    pub task: String,
    pub output: String,
    pub amount_sats: u64,
    pub unit: String,
    pub deadline_unix: u64,
    pub mint_url: String,
    pub seller_pubkey: Option<String>,
}

impl ParsedOffer {
    pub fn is_targeted(&self) -> bool {
        self.seller_pubkey.is_some()
    }

    pub fn seller_matches(&self, seller_pubkey: &str) -> bool {
        match self.seller_pubkey.as_deref() {
            Some(target) => target == seller_pubkey,
            None => true,
        }
    }

    pub fn assert_seller_matches(&self, seller_pubkey: &str) -> Result<(), TargetingError> {
        match self.seller_pubkey.as_deref() {
            Some(target) if target != seller_pubkey => Err(TargetingError {
                expected: target.to_owned(),
                actual: seller_pubkey.to_owned(),
            }),
            _ => Ok(()),
        }
    }
}

pub fn is_targeted(offer: &ParsedOffer) -> bool {
    offer.is_targeted()
}

pub fn assert_seller_matches(
    offer: &ParsedOffer,
    seller_pubkey: &str,
) -> Result<(), TargetingError> {
    offer.assert_seller_matches(seller_pubkey)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetingError {
    pub expected: String,
    pub actual: String,
}

impl fmt::Display for TargetingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "offer targets seller {}, not {}",
            self.expected, self.actual
        )
    }
}

impl std::error::Error for TargetingError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OfferParseError {
    WrongKind(u16),
    MissingTag(&'static str),
    InvalidAmount(String),
    InvalidDeadline(String),
    UnsupportedUnit(String),
    UnsupportedVersion(String),
    MissingMobeeTag,
}

impl fmt::Display for OfferParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WrongKind(kind) => write!(f, "expected kind {JOB_OFFER_KIND}, got {kind}"),
            Self::MissingTag(tag) => write!(f, "missing required tag {tag}"),
            Self::InvalidAmount(value) => write!(f, "invalid amount tag value {value:?}"),
            Self::InvalidDeadline(value) => write!(f, "invalid deadline tag value {value:?}"),
            Self::UnsupportedUnit(unit) => write!(f, "unsupported amount unit {unit:?}"),
            Self::UnsupportedVersion(version) => write!(f, "unsupported mobee version {version:?}"),
            Self::MissingMobeeTag => write!(f, "missing t=mobee tag"),
        }
    }
}

impl std::error::Error for OfferParseError {}

pub fn parse_offer(event: &EventDraft) -> Result<ParsedOffer, OfferParseError> {
    if event.kind != JOB_OFFER_KIND {
        return Err(OfferParseError::WrongKind(event.kind));
    }
    if !has_tag_value(&event.tags, "t", MOBEE_TAG) {
        return Err(OfferParseError::MissingMobeeTag);
    }
    let version = first_tag_value(&event.tags, "v").ok_or(OfferParseError::MissingTag("v"))?;
    if version != PROTOCOL_VERSION {
        return Err(OfferParseError::UnsupportedVersion(version.to_owned()));
    }

    let amount_tag =
        first_tag(&event.tags, "amount").ok_or(OfferParseError::MissingTag("amount"))?;
    let amount_value = amount_tag
        .0
        .get(1)
        .ok_or(OfferParseError::MissingTag("amount"))?;
    let unit = amount_tag
        .0
        .get(2)
        .ok_or(OfferParseError::MissingTag("amount unit"))?;
    if unit != "sat" {
        return Err(OfferParseError::UnsupportedUnit(unit.clone()));
    }
    let amount_sats = amount_value
        .parse()
        .map_err(|_| OfferParseError::InvalidAmount(amount_value.clone()))?;

    let deadline = event
        .tags
        .iter()
        .find(|tag| {
            tag.0.first().map(String::as_str) == Some("param")
                && tag.0.get(1).map(String::as_str) == Some("deadline")
        })
        .and_then(|tag| tag.0.get(2))
        .ok_or(OfferParseError::MissingTag("param deadline"))?;
    let deadline_unix = deadline
        .parse()
        .map_err(|_| OfferParseError::InvalidDeadline(deadline.clone()))?;

    Ok(ParsedOffer {
        task: first_tag_value(&event.tags, "i")
            .ok_or(OfferParseError::MissingTag("i"))?
            .to_owned(),
        output: first_tag_value(&event.tags, "output")
            .ok_or(OfferParseError::MissingTag("output"))?
            .to_owned(),
        amount_sats,
        unit: unit.clone(),
        deadline_unix,
        mint_url: first_tag_value(&event.tags, "mint")
            .ok_or(OfferParseError::MissingTag("mint"))?
            .to_owned(),
        seller_pubkey: first_tag_value(&event.tags, "p").map(str::to_owned),
    })
}

pub fn claim_draft(offer_id: &str, buyer_pubkey: &str, seller_pubkey: &str) -> EventDraft {
    feedback_draft(
        "processing",
        vec![
            TagSpec::new(["e", offer_id]),
            TagSpec::new(["p", buyer_pubkey]),
            TagSpec::new(["p", seller_pubkey]),
        ],
    )
}

pub fn accept_draft(
    offer_id: &str,
    claim_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
) -> EventDraft {
    feedback_draft(
        "accepted",
        vec![
            TagSpec::new(["e", offer_id, "", "root"]),
            TagSpec::new(["e", claim_id]),
            TagSpec::new(["p", buyer_pubkey]),
            TagSpec::new(["p", seller_pubkey]),
        ],
    )
}

pub fn result_draft(
    offer_id: &str,
    buyer_pubkey: &str,
    output: &str,
    amount_sats: u64,
    job_hash: &str,
    seller_signature: &str,
    content: impl Into<String>,
) -> EventDraft {
    EventDraft::new(
        JOB_RESULT_KIND,
        vec![
            TagSpec::new(["e", offer_id, "", "root"]),
            TagSpec::new(["p", buyer_pubkey]),
            TagSpec::new(["output", output]),
            TagSpec::new(["amount", &amount_sats.to_string(), "sat"]),
            TagSpec::new(["job-hash", job_hash]),
            TagSpec::new(["sig", "seller", seller_signature]),
            mobee_tag(),
            version_tag(),
        ],
        content,
    )
}

pub fn receipt_draft(
    offer_id: &str,
    result_id: &str,
    buyer_pubkey: &str,
    seller_pubkey: &str,
    mint_url: &str,
    amount_sats: u64,
    job_hash: &str,
    seller_signature: &str,
    buyer_signature: &str,
) -> EventDraft {
    EventDraft::new(
        JOB_RECEIPT_KIND,
        vec![
            TagSpec::new(["job-hash", job_hash]),
            TagSpec::new(["amount", &amount_sats.to_string(), "sat"]),
            TagSpec::new(["e", offer_id, "", "root"]),
            TagSpec::new(["e", result_id, "", "reply"]),
            TagSpec::new(["p", buyer_pubkey]),
            TagSpec::new(["p", seller_pubkey]),
            TagSpec::new(["mint", mint_url]),
            TagSpec::new(["sig", "seller", seller_signature]),
            TagSpec::new(["sig", "buyer", buyer_signature]),
            mobee_tag(),
            version_tag(),
        ],
        "",
    )
}

fn feedback_draft(status: &str, mut tags: Vec<TagSpec>) -> EventDraft {
    tags.insert(0, TagSpec::new(["status", status]));
    tags.push(mobee_tag());
    tags.push(version_tag());
    EventDraft::new(JOB_FEEDBACK_KIND, tags, "")
}

fn first_tag<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a TagSpec> {
    tags.iter()
        .find(|tag| tag.0.first().map(String::as_str) == Some(name))
}

fn first_tag_value<'a>(tags: &'a [TagSpec], name: &str) -> Option<&'a str> {
    first_tag(tags, name).and_then(TagSpec::value)
}

fn has_tag_value(tags: &[TagSpec], name: &str, value: &str) -> bool {
    tags.iter().any(|tag| {
        tag.0.first().map(String::as_str) == Some(name)
            && tag.0.get(1).map(String::as_str) == Some(value)
    })
}

fn mobee_tag() -> TagSpec {
    TagSpec::new(["t", MOBEE_TAG])
}

fn version_tag() -> TagSpec {
    TagSpec::new(["v", PROTOCOL_VERSION])
}

#[cfg(feature = "gateway")]
pub mod nostr {
    use nostr_sdk::prelude::{EventBuilder, Kind, Tag};

    use super::{EventDraft, TagSpec};

    pub fn event_builder(
        draft: &EventDraft,
    ) -> Result<EventBuilder, nostr_sdk::prelude::tag::Error> {
        let mut builder = EventBuilder::new(Kind::Custom(draft.kind), draft.content.clone());
        builder.allow_self_tagging = true;
        for tag in &draft.tags {
            builder = builder.tag(to_tag(tag)?);
        }
        Ok(builder)
    }

    fn to_tag(tag: &TagSpec) -> Result<Tag, nostr_sdk::prelude::tag::Error> {
        Tag::parse(tag.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUYER: &str = "buyer";
    const SELLER: &str = "seller";
    const OTHER_SELLER: &str = "other-seller";
    const TESTNUT_MINT_URL: &str = "https://testnut.cashu.space";

    #[test]
    fn offer_draft_uses_locked_job_microstandard_tags() {
        let draft = OfferDraft::new(
            "write hello.txt",
            "text/plain",
            7,
            1_800_000_000,
            TESTNUT_MINT_URL,
            SELLER,
        )
        .to_event_draft();

        assert_eq!(draft.kind, JOB_OFFER_KIND);
        assert_eq!(draft.content, "");
        assert_eq!(
            draft.tags,
            vec![
                TagSpec::new(["i", "write hello.txt"]),
                TagSpec::new(["output", "text/plain"]),
                TagSpec::new(["amount", "7", "sat"]),
                TagSpec::new(["param", "deadline", "1800000000"]),
                TagSpec::new(["mint", TESTNUT_MINT_URL]),
                TagSpec::new(["p", SELLER]),
                TagSpec::new(["t", MOBEE_TAG]),
                TagSpec::new(["v", PROTOCOL_VERSION]),
            ]
        );
    }

    #[test]
    fn untargeted_offer_draft_omits_seller_tag() {
        let draft = OfferDraft::untargeted(
            "write hello.txt",
            "text/plain",
            7,
            1_800_000_000,
            TESTNUT_MINT_URL,
        )
        .to_event_draft();

        assert_eq!(draft.kind, JOB_OFFER_KIND);
        assert!(!has_tag_value(&draft.tags, "p", SELLER));
        assert_eq!(
            parse_offer(&draft).expect("parse offer").seller_pubkey,
            None
        );
    }

    #[test]
    fn parse_offer_round_trips_locked_tags() {
        let draft = OfferDraft::new(
            "summarize",
            "application/json",
            3,
            1_800_000_001,
            TESTNUT_MINT_URL,
            SELLER,
        )
        .to_event_draft();

        assert_eq!(
            parse_offer(&draft).expect("parse offer"),
            ParsedOffer {
                task: "summarize".into(),
                output: "application/json".into(),
                amount_sats: 3,
                unit: "sat".into(),
                deadline_unix: 1_800_000_001,
                mint_url: TESTNUT_MINT_URL.into(),
                seller_pubkey: Some(SELLER.into()),
            }
        );
    }

    #[test]
    fn targeting_helpers_fail_closed_for_targeted_offers() {
        let targeted = parse_offer(
            &OfferDraft::new("task", "text/plain", 1, 2, TESTNUT_MINT_URL, SELLER).to_event_draft(),
        )
        .expect("targeted offer");
        let untargeted = parse_offer(
            &OfferDraft::untargeted("task", "text/plain", 1, 2, TESTNUT_MINT_URL).to_event_draft(),
        )
        .expect("untargeted offer");

        assert!(is_targeted(&targeted));
        assert!(!is_targeted(&untargeted));
        assert!(targeted.seller_matches(SELLER));
        assert!(!targeted.seller_matches(OTHER_SELLER));
        assert!(untargeted.seller_matches(OTHER_SELLER));
        assert_seller_matches(&targeted, SELLER).expect("matching seller");
        assert_seller_matches(&untargeted, OTHER_SELLER).expect("untargeted seller");
        assert_eq!(
            assert_seller_matches(&targeted, OTHER_SELLER),
            Err(TargetingError {
                expected: SELLER.into(),
                actual: OTHER_SELLER.into(),
            })
        );
    }

    #[test]
    fn claim_and_accept_use_kind_7000_status_tags() {
        assert_eq!(
            claim_draft("offer", BUYER, SELLER),
            EventDraft::new(
                JOB_FEEDBACK_KIND,
                vec![
                    TagSpec::new(["status", "processing"]),
                    TagSpec::new(["e", "offer"]),
                    TagSpec::new(["p", BUYER]),
                    TagSpec::new(["p", SELLER]),
                    TagSpec::new(["t", MOBEE_TAG]),
                    TagSpec::new(["v", PROTOCOL_VERSION]),
                ],
                ""
            )
        );

        assert_eq!(
            accept_draft("offer", "claim", BUYER, SELLER),
            EventDraft::new(
                JOB_FEEDBACK_KIND,
                vec![
                    TagSpec::new(["status", "accepted"]),
                    TagSpec::new(["e", "offer", "", "root"]),
                    TagSpec::new(["e", "claim"]),
                    TagSpec::new(["p", BUYER]),
                    TagSpec::new(["p", SELLER]),
                    TagSpec::new(["t", MOBEE_TAG]),
                    TagSpec::new(["v", PROTOCOL_VERSION]),
                ],
                ""
            )
        );
    }

    #[test]
    fn result_and_receipt_keep_market_tags_outside_driver() {
        let result = result_draft(
            "offer",
            BUYER,
            "text/plain",
            7,
            "hash",
            "seller-sig",
            "done",
        );
        assert_eq!(result.kind, JOB_RESULT_KIND);
        assert_eq!(result.content, "done");
        assert!(has_tag_value(&result.tags, "job-hash", "hash"));
        assert!(has_tag_value_at(&result.tags, "sig", 1, "seller"));
        assert!(has_tag_value_at(&result.tags, "sig", 2, "seller-sig"));

        let receipt = receipt_draft(
            "offer",
            "result",
            BUYER,
            SELLER,
            TESTNUT_MINT_URL,
            7,
            "hash",
            "seller-sig",
            "buyer-sig",
        );
        assert_eq!(receipt.kind, JOB_RECEIPT_KIND);
        assert!(has_tag_value(&receipt.tags, "mint", TESTNUT_MINT_URL));
        assert!(has_tag_value_at(&receipt.tags, "e", 1, "result"));
        assert!(has_tag_value_at(&receipt.tags, "e", 3, "reply"));
        assert_eq!(
            receipt
                .tags
                .iter()
                .filter(|tag| tag.first() == Some("sig"))
                .count(),
            2
        );
        assert!(has_tag_value_at(&receipt.tags, "sig", 1, "seller"));
        assert!(has_tag_value_at(&receipt.tags, "sig", 1, "buyer"));
    }

    fn has_tag_value_at(tags: &[TagSpec], name: &str, index: usize, value: &str) -> bool {
        tags.iter().any(|tag| {
            tag.0.first().map(String::as_str) == Some(name)
                && tag.0.get(index).map(String::as_str) == Some(value)
        })
    }
}
