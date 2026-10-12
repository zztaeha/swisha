use swisha::domain::errors::{Category, Language, TABLE, UNKNOWN, UNKNOWN_OUTCOME, describe, describe_failure};

#[test]
fn every_code_in_the_table_resolves() {
    for (code, expected) in TABLE {
        assert_eq!(describe(Some(code)), *expected, "code {code}");
    }
}

#[test]
fn the_table_has_no_duplicate_codes() {
    let mut codes: Vec<&str> = TABLE.iter().map(|(c, _)| *c).collect();
    let total = codes.len();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), total, "duplicate code in the table");
}

#[test]
fn every_entry_has_both_languages_filled_in() {
    for (code, info) in TABLE {
        assert!(!info.english.is_empty(), "{code} is missing English");
        assert!(!info.swedish.is_empty(), "{code} is missing Swedish");
    }
}

#[test]
fn unknown_and_missing_codes_fall_back() {
    assert_eq!(describe(None), UNKNOWN);
    assert_eq!(describe(Some("")), UNKNOWN);
    assert_eq!(describe(Some("   ")), UNKNOWN);
    assert_eq!(describe(Some("ZZ99")), UNKNOWN);
}

#[test]
fn lookup_tolerates_whitespace_and_case() {
    assert_eq!(describe(Some(" RF07 ")), describe(Some("RF07")));
    assert_eq!(describe(Some("rf07")), describe(Some("RF07")));
}

#[test]
fn declines_the_customer_can_act_on_are_user_fixable() {
    for code in ["RF07", "ACMT07", "ACMT17", "VR02", "PA06", "AM02", "AM06", "CD01"] {
        assert_eq!(describe(Some(code)).category, Category::UserFixable, "code {code}");
    }
}

#[test]
fn merchant_misconfiguration_is_never_retryable() {
    for code in ["RP01", "RP02", "RP03", "FF08", "PA01", "PA02", "401", "403", "ACMT03"] {
        assert_eq!(describe(Some(code)).category, Category::ContactSupport, "code {code}");
    }
}

#[test]
fn language_selection_picks_the_matching_string() {
    let rf07 = describe(Some("RF07"));
    assert_eq!(rf07.message(Language::English), rf07.english);
    assert_eq!(rf07.message(Language::Swedish), rf07.swedish);
    assert!(rf07.message(Language::English).contains("declined"));
    assert!(rf07.message(Language::Swedish).contains("nekades"));
}

#[test]
fn language_parses_from_config_values_and_defaults_to_english() {
    for v in ["sv", "SV", "se", "sv-SE", "swedish", "svenska", " sv "] {
        assert_eq!(Language::parse(v), Language::Swedish, "value {v:?}");
    }
    for v in ["en", "english", "", "nonsense", "de"] {
        assert_eq!(Language::parse(v), Language::English, "value {v:?}");
    }
    assert_eq!(Language::default(), Language::English);
}

// swisha never resubmits a payout on its own, so Retryable is advice for a person rather than
// permission for the server. These are the codes a UI should offer a retry button for.
#[test]
fn transient_swish_failures_are_categorized_retryable() {
    for code in ["TA01", "TM01", "FF10", "RP09", "429", "500"] {
        assert_eq!(describe(Some(code)).category, Category::Retryable, "code {code}");
    }
}

// swisha does not retry a payout, in any circumstance. A message promising that it will would
// leave a person waiting for something that never happens, which is worse than saying nothing.
// The identifier scan in no_resubmission.rs cannot catch this: these are prose, not symbols.
#[test]
fn no_message_promises_that_swisha_acts_on_its_own() {
    for (code, info) in TABLE {
        for text in [info.english, info.swedish] {
            let lower = text.to_lowercase();
            for claim in ["automatically", "automatiskt", "retrying", "försöker igen"] {
                assert!(
                    !lower.contains(claim),
                    "{code} tells the reader swisha will act on its own: {text:?}"
                );
            }
        }
    }
}

// A caller renders these straight into a UI, so both languages have to be finished sentences.
#[test]
fn every_message_is_a_complete_sentence_in_both_languages() {
    for (code, info) in TABLE {
        for (lang, text) in [("english", info.english), ("swedish", info.swedish)] {
            assert!(text.ends_with('.'), "{code} {lang} does not end in a period: {text:?}");
            assert!(
                text.chars().next().is_some_and(char::is_uppercase),
                "{code} {lang} does not start with a capital: {text:?}"
            );
            assert!(!text.contains("  "), "{code} {lang} has a doubled space");
        }
    }
}

// These three strings cross the wire as `error_category` and a caller branches on them, so they
// are API rather than an implementation detail.
#[test]
fn category_strings_are_stable() {
    assert_eq!(Category::Retryable.as_str(), "retryable");
    assert_eq!(Category::UserFixable.as_str(), "user_fixable");
    assert_eq!(Category::ContactSupport.as_str(), "contact_support");
}

#[test]
fn every_category_renders_to_something_distinct() {
    let all = [Category::Retryable, Category::UserFixable, Category::ContactSupport];
    let mut rendered: Vec<&str> = all.iter().map(|c| c.as_str()).collect();
    rendered.sort_unstable();
    rendered.dedup();
    assert_eq!(rendered.len(), all.len(), "two categories render the same string");
    assert!(rendered.iter().all(|s| !s.is_empty()));
}

// A failed payout must carry a description even when Swish supplied no code, or the status
// endpoint answers "it failed" with nothing a person can act on.
#[test]
fn a_failure_is_always_explained_even_without_a_code() {
    assert_eq!(describe_failure("DECLINED", None), Some(UNKNOWN), "Swish refused it: nothing moved");
    assert_eq!(describe_failure("ERROR", None), Some(UNKNOWN), "Swish said it failed");
    assert_eq!(describe_failure("NEEDS_REVIEW", None), Some(UNKNOWN_OUTCOME), "no answer at all");
}

// NEEDS_REVIEW means Swish never answered and may hold the payout, whatever code its send left,
// so it never reads "failed" or "try again". ERROR is Swish's own word that the payout failed.
#[test]
fn an_unknown_outcome_never_reads_as_failed() {
    for code in [None, Some("503"), Some("XX99"), Some("500"), Some("RP09"), Some("429"), Some("TA01")] {
        let info = describe_failure("NEEDS_REVIEW", code).expect("explained");
        assert_eq!(info, UNKNOWN_OUTCOME, "{code:?}");
        for language in [Language::English, Language::Swedish] {
            let message = info.message(language).to_lowercase();
            for invitation in ["failed", "misslyckades", "try again", "försök igen"] {
                assert!(!message.contains(invitation), "{code:?}: {message}");
            }
        }
    }
    assert_eq!(describe_failure("ERROR", Some("XX99")), Some(UNKNOWN), "Swish said it failed");
    assert_eq!(describe_failure("DECLINED", Some("XX99")), Some(UNKNOWN), "nothing was stored");
    assert_eq!(describe_failure("ERROR", Some("RF07")), Some(describe(Some("RF07"))), "a known code speaks");
}

#[test]
fn a_settled_payout_carries_no_failure_text() {
    for status in ["PAID", "DEBITED", "CREATED", "PENDING"] {
        assert_eq!(describe_failure(status, None), None, "{status} has not failed");
    }
}

// A code in the trail outlives the failure it described: Swish can refuse with TA01 and then pay
// the same instruction. On a settled payout that code must not surface as advice to pay again.
#[test]
fn a_code_on_a_settled_status_is_not_explained() {
    for status in ["PAID", "DEBITED", "CREATED", "PENDING"] {
        assert_eq!(describe_failure(status, Some("TA01")), None, "{status} has not failed");
    }
}

// The review texts go straight into a UI beside a payout, so they meet the error table's bar:
// finished sentences in both languages, and none promising that swisha will act on its own.
#[test]
fn every_review_text_is_a_finished_sentence_that_leaves_the_decision_to_a_person() {
    use swisha::domain::review::{NO_OUTCOME, RETURNED, STILL_FAILED, UNCONFIRMED};
    for review in [RETURNED, UNCONFIRMED, STILL_FAILED, NO_OUTCOME] {
        for text in [review.english, review.swedish] {
            assert!(text.ends_with('.') && text.chars().next().is_some_and(char::is_uppercase), "{text}");
            assert!(!text.contains("  "), "{text}");
            for claim in ["automatically", "automatiskt", "swisha will", "swisha kommer"] {
                assert!(!text.to_lowercase().contains(claim), "{text}");
            }
        }
    }
}
