use chrono::{DateTime, Duration, NaiveDate, Utc};
use property::{
    store::{ApplyRequest, ObservationKind, Store},
    tax::{
        ledger::{
            Allocation, Assessment, Balance, BalanceScope, BillIdentity, BillKind, BindingKey,
            CountyFips, Coverage, CoverageCapability, CoverageStatus, Evidence, ImportInput,
            Installment, Notice, Observation, ObservationData, ParcelId, Payment, PaymentStatus,
            PropertyBinding, Snapshot, SourceKind, TaxError,
        },
        reconcile::{InstallmentState, Reconciliation, reconcile},
    },
};

fn now() -> DateTime<Utc> {
    "2030-11-15T12:00:00Z".parse().unwrap()
}

fn binding() -> PropertyBinding {
    PropertyBinding {
        key: BindingKey {
            jurisdiction: CountyFips::los_angeles(),
            parcel: ParcelId::try_from("FICTIONAL-PARCEL-A".to_owned()).unwrap(),
        },
        label: "Example property".to_owned(),
        confirmed_at: now(),
        confirmation: evidence(SourceKind::County, "fictional-parcel-confirmation", now()),
    }
}

fn evidence(source: SourceKind, document: &str, effective_at: DateTime<Utc>) -> Evidence {
    Evidence {
        source,
        document: document.to_owned(),
        url: None,
        observed_at: effective_at,
        effective_at,
    }
}

fn identity(sequence: &str, source_version: &str) -> BillIdentity {
    let key = binding().key;
    BillIdentity {
        jurisdiction: key.jurisdiction,
        parcel: key.parcel,
        fiscal_year: 2030,
        sequence: sequence.to_owned(),
        source_version: source_version.to_owned(),
    }
}

fn observation(id: &str, data: ObservationData, at: DateTime<Utc>) -> Observation {
    Observation {
        id: id.to_owned(),
        binding: binding().key,
        evidence: evidence(SourceKind::County, id, at),
        observation: data,
    }
}

fn notice(id: &str, bill: &BillIdentity, bill_kind: BillKind) -> Observation {
    observation(
        id,
        ObservationData::Notice(Notice {
            identity: bill.clone(),
            bill_kind,
            supersedes: None,
            installments: vec![
                Installment {
                    number: 1,
                    amount_cents: 25_000,
                    due_date: NaiveDate::from_ymd_opt(2030, 11, 1).unwrap(),
                    delinquent_at: "2030-12-11T01:00:00Z".parse().unwrap(),
                    timezone: chrono_tz::America::Los_Angeles,
                },
                Installment {
                    number: 2,
                    amount_cents: 25_000,
                    due_date: NaiveDate::from_ymd_opt(2031, 2, 1).unwrap(),
                    delinquent_at: "2031-04-11T00:00:00Z".parse().unwrap(),
                    timezone: chrono_tz::America::Los_Angeles,
                },
            ],
        }),
        now() - Duration::days(20),
    )
}

fn payment(id: &str, status: PaymentStatus, allocations: Vec<Allocation>) -> Observation {
    let amount_cents = allocations.iter().map(|item| item.amount_cents).sum();
    observation(
        id,
        ObservationData::Payment(Payment {
            reference: "FICTIONAL-PAYMENT-1".to_owned(),
            status,
            amount_cents,
            allocations,
        }),
        now(),
    )
}

fn allocation(bill: &BillIdentity, installment: u8) -> Allocation {
    Allocation {
        bill: bill.clone(),
        installment,
        amount_cents: 25_000,
    }
}

fn balance(
    id: &str,
    scope: BalanceScope,
    amount_cents: Option<i64>,
    at: DateTime<Utc>,
) -> Observation {
    observation(
        id,
        ObservationData::Balance(Balance {
            scope,
            amount_cents,
        }),
        at,
    )
}

fn import(observations: Vec<Observation>) -> ImportInput {
    ImportInput {
        property: binding(),
        observations,
        coverage: vec![],
    }
}

fn snapshot(observations: Vec<Observation>) -> Snapshot {
    Snapshot {
        revision: 1,
        property: Some(binding()),
        observations,
        coverage: vec![],
    }
}

fn installment(result: &Reconciliation, bill: &BillIdentity, number: u8) -> InstallmentState {
    result
        .installments
        .iter()
        .find(|item| &item.identity == bill && item.number == number)
        .unwrap()
        .state
        .clone()
}

#[test]
fn one_payment_allocated_to_two_first_installments_never_closes_either_second() {
    let annual = identity("annual", "original");
    let supplemental = identity("supplemental-1", "original");
    let result = reconcile(
        &snapshot(vec![
            notice("annual-notice", &annual, BillKind::Annual),
            notice("supplemental-notice", &supplemental, BillKind::Supplemental),
            payment(
                "county-posted",
                PaymentStatus::CountyPosted,
                vec![allocation(&annual, 1), allocation(&supplemental, 1)],
            ),
        ]),
        now(),
    );
    assert_eq!(result.bills.len(), 2);
    for bill in [&annual, &supplemental] {
        assert_eq!(installment(&result, bill, 1), InstallmentState::Paid);
        assert_eq!(installment(&result, bill, 2), InstallmentState::Open);
    }
    assert_eq!(result.known_balance_cents, None);
}

#[test]
fn bank_settlement_is_pending_and_missing_notices_remain_unresolved() {
    let annual = identity("annual", "original");
    let missing = identity("supplemental-missing", "original");
    let mut receipt = payment(
        "bank-receipt",
        PaymentStatus::BankSettled,
        vec![allocation(&annual, 1), allocation(&missing, 1)],
    );
    receipt.evidence.source = SourceKind::Bank;
    let result = reconcile(
        &snapshot(vec![notice("notice", &annual, BillKind::Annual), receipt]),
        now(),
    );
    assert_eq!(
        installment(&result, &annual, 1),
        InstallmentState::PendingPayment
    );
    let unresolved = result
        .bills
        .iter()
        .find(|bill| bill.identity == missing)
        .unwrap();
    assert!(unresolved.missing_notice);
    assert!(unresolved.installments.is_empty());
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "missing_notice")
    );
    assert_eq!(result.known_balance_cents, None);
}

#[test]
fn reversal_or_refund_removes_credit_without_erasing_its_history() {
    let bill = identity("annual", "original");
    for status in [PaymentStatus::Reversed, PaymentStatus::Refunded] {
        let mut posted = payment(
            "posted",
            PaymentStatus::CountyPosted,
            vec![allocation(&bill, 1)],
        );
        posted.evidence.effective_at -= Duration::hours(1);
        let returned = payment("returned", status, vec![allocation(&bill, 1)]);
        let input = snapshot(vec![
            notice("notice", &bill, BillKind::Annual),
            posted,
            returned,
        ]);
        let result = reconcile(&input, now());
        assert_eq!(installment(&result, &bill, 1), InstallmentState::Open);
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.code == "payment_returned")
        );
        assert_eq!(result.payment_count, 1);
        assert_eq!(input.observations.len(), 3);
    }
}

#[test]
fn a_zero_balance_before_a_return_does_not_keep_the_installment_paid() {
    let bill = identity("annual", "original");
    let result = reconcile(
        &snapshot(vec![
            notice("notice", &bill, BillKind::Annual),
            balance(
                "earlier-zero",
                BalanceScope::Installment {
                    bill: bill.clone(),
                    number: 1,
                },
                Some(0),
                now() - Duration::hours(1),
            ),
            payment(
                "returned",
                PaymentStatus::Reversed,
                vec![allocation(&bill, 1)],
            ),
        ]),
        now(),
    );
    assert_ne!(installment(&result, &bill, 1), InstallmentState::Paid);
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "contradiction")
    );
}

#[test]
fn an_older_property_zero_is_unknown_after_a_later_returned_payment() {
    let bill = identity("annual", "original");
    let result = reconcile(
        &snapshot(vec![
            notice("notice", &bill, BillKind::Annual),
            balance(
                "earlier-property-zero",
                BalanceScope::Property,
                Some(0),
                now() - Duration::hours(1),
            ),
            payment(
                "returned",
                PaymentStatus::Refunded,
                vec![allocation(&bill, 1)],
            ),
        ]),
        now(),
    );
    assert_eq!(result.known_balance_cents, None);
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "balance_outdated")
    );
}

#[test]
fn scopes_and_freshness_prevent_false_zero_and_detect_contradictions() {
    let bill = identity("annual", "original");
    let mut input = snapshot(vec![
        notice("notice", &bill, BillKind::Annual),
        balance(
            "whole-bill-zero",
            BalanceScope::Bill { bill: bill.clone() },
            Some(0),
            now(),
        ),
        balance(
            "stale-property-zero",
            BalanceScope::Property,
            Some(0),
            now() - Duration::days(3),
        ),
    ]);
    let result = reconcile(&input, now());
    assert_eq!(result.known_balance_cents, None);
    assert_eq!(installment(&result, &bill, 1), InstallmentState::Open);
    assert_eq!(installment(&result, &bill, 2), InstallmentState::Open);
    input.observations.push(balance(
        "scope-positive",
        BalanceScope::Installment {
            bill: bill.clone(),
            number: 1,
        },
        Some(100),
        now(),
    ));
    input.observations.push(balance(
        "scope-zero",
        BalanceScope::Installment {
            bill: bill.clone(),
            number: 1,
        },
        Some(0),
        now(),
    ));
    input.observations.push(balance(
        "property-zero",
        BalanceScope::Property,
        Some(0),
        now(),
    ));
    input.observations.push(balance(
        "property-positive",
        BalanceScope::Property,
        Some(200),
        now(),
    ));
    let result = reconcile(&input, now());
    assert_eq!(
        installment(&result, &bill, 1),
        InstallmentState::Contradiction
    );
    assert_eq!(result.known_balance_cents, None);
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "balance_conflict")
    );
}

#[test]
fn review_regression_property_zero_conflicts_with_a_same_time_positive_installment() {
    let bill = identity("annual", "original");
    let data = vec![
        notice("notice", &bill, BillKind::Annual),
        balance("property-zero", BalanceScope::Property, Some(0), now()),
        balance(
            "installment-positive",
            BalanceScope::Installment {
                bill: bill.clone(),
                number: 1,
            },
            Some(10_000),
            now(),
        ),
    ];
    import(data.clone()).validate().unwrap();
    let result = reconcile(&snapshot(data.clone()), now());
    assert_eq!(result.known_balance_cents, None);
    assert_eq!(
        installment(&result, &bill, 1),
        InstallmentState::Contradiction
    );
    assert_eq!(installment(&result, &bill, 2), InstallmentState::Open);
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "balance_conflict")
    );
    let conflict = result
        .issues
        .iter()
        .find(|issue| issue.code == "balance_conflict")
        .unwrap();
    assert!(
        conflict
            .observation_ids
            .contains(&"property-zero".to_owned())
    );
    assert!(
        conflict
            .observation_ids
            .contains(&"installment-positive".to_owned())
    );
    let mut earlier_child = data;
    earlier_child[2].evidence.effective_at -= Duration::minutes(1);
    let result = reconcile(&snapshot(earlier_child), now());
    assert_eq!(result.known_balance_cents, Some(0));
    assert!(
        !result
            .issues
            .iter()
            .any(|issue| issue.code == "balance_conflict")
    );
}

#[test]
fn review_regression_return_without_allocations_preserves_the_previous_installment_link() {
    let annual = identity("annual", "original");
    let supplemental = identity("supplemental", "original");
    for status in [PaymentStatus::Reversed, PaymentStatus::Refunded] {
        let mut posted = payment(
            "posted",
            PaymentStatus::CountyPosted,
            vec![allocation(&annual, 1), allocation(&supplemental, 1)],
        );
        posted.evidence.effective_at -= Duration::minutes(20);
        let mut returned = payment(
            "returned-without-allocations",
            status,
            vec![allocation(&annual, 1), allocation(&supplemental, 1)],
        );
        if let ObservationData::Payment(payment) = &mut returned.observation {
            payment.allocations.clear();
        }
        let data = vec![
            notice("annual-notice", &annual, BillKind::Annual),
            notice("supplemental-notice", &supplemental, BillKind::Supplemental),
            posted,
            balance(
                "annual-zero",
                BalanceScope::Installment {
                    bill: annual.clone(),
                    number: 1,
                },
                Some(0),
                now() - Duration::minutes(10),
            ),
            balance(
                "supplemental-zero",
                BalanceScope::Installment {
                    bill: supplemental.clone(),
                    number: 1,
                },
                Some(0),
                now() - Duration::minutes(10),
            ),
            returned,
        ];
        import(data.clone()).validate().unwrap();
        let result = reconcile(&snapshot(data.clone()), now());
        for bill in [&annual, &supplemental] {
            assert_eq!(
                installment(&result, bill, 1),
                InstallmentState::Contradiction
            );
            assert_eq!(installment(&result, bill, 2), InstallmentState::Open);
            let evidence = &result
                .installments
                .iter()
                .find(|item| &item.identity == bill && item.number == 1)
                .unwrap()
                .evidence;
            assert!(evidence.contains(&"posted".to_owned()));
            assert!(evidence.contains(&"returned-without-allocations".to_owned()));
        }
        assert!(
            result
                .issues
                .iter()
                .any(|issue| issue.code == "payment_returned")
        );
        let mut refreshed = data;
        refreshed.push(balance(
            "new-county-zero",
            BalanceScope::Installment {
                bill: annual.clone(),
                number: 1,
            },
            Some(0),
            now() + Duration::minutes(1),
        ));
        let result = reconcile(&snapshot(refreshed), now() + Duration::minutes(2));
        assert_eq!(installment(&result, &annual, 1), InstallmentState::Paid);
        assert_eq!(
            installment(&result, &supplemental, 1),
            InstallmentState::Contradiction
        );
    }
}

#[test]
fn review_regression_conflicting_adjusted_notices_cannot_hide_original_deadlines() {
    let original = identity("annual", "original");
    let adjusted = identity("annual", "adjusted");
    let mut first = notice("adjustment-one", &adjusted, BillKind::Annual);
    if let ObservationData::Notice(notice) = &mut first.observation {
        notice.supersedes = Some(original.clone());
    }
    let mut conflicting = first.clone();
    conflicting.id = "adjustment-conflict".to_owned();
    if let ObservationData::Notice(notice) = &mut conflicting.observation {
        notice.installments[0].amount_cents += 1_000;
    }
    let data = vec![
        notice("original", &original, BillKind::Annual),
        first,
        conflicting,
    ];
    import(data.clone()).validate().unwrap();
    let result = reconcile(&snapshot(data.clone()), now());
    assert!(
        !result
            .bills
            .iter()
            .find(|bill| bill.identity == original)
            .unwrap()
            .superseded
    );
    assert_eq!(installment(&result, &original, 1), InstallmentState::Open);
    assert_eq!(installment(&result, &original, 2), InstallmentState::Open);
    assert_eq!(
        installment(&result, &adjusted, 1),
        InstallmentState::Contradiction
    );
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "conflicting_supersession")
    );
    let mut consistent = data;
    if let ObservationData::Notice(notice) = &mut consistent[2].observation {
        notice.installments[0].amount_cents -= 1_000;
    }
    let result = reconcile(&snapshot(consistent), now());
    assert!(
        result
            .bills
            .iter()
            .find(|bill| bill.identity == original)
            .unwrap()
            .superseded
    );
    assert_eq!(installment(&result, &adjusted, 1), InstallmentState::Open);
}

#[test]
fn explicit_unknown_and_owner_balances_never_become_authoritative_zero() {
    let mut unknown = balance("unknown", BalanceScope::Property, None, now());
    let mut owner = balance("owner-zero", BalanceScope::Property, Some(0), now());
    owner.evidence.source = SourceKind::OwnerDocument;
    let result = reconcile(&snapshot(vec![unknown.clone(), owner]), now());
    assert_eq!(result.known_balance_cents, None);
    unknown.evidence.effective_at = now() + Duration::days(10);
    if let ObservationData::Balance(data) = &mut unknown.observation {
        data.amount_cents = Some(0);
    }
    let result = reconcile(&snapshot(vec![unknown]), now());
    assert_eq!(result.known_balance_cents, None);
}

#[test]
fn lifecycle_downgrade_or_unallocated_money_cannot_close_a_bill() {
    let bill = identity("annual", "original");
    let mut posted = payment(
        "posted",
        PaymentStatus::CountyPosted,
        vec![allocation(&bill, 1)],
    );
    posted.evidence.effective_at -= Duration::hours(1);
    let submitted = payment(
        "downgraded",
        PaymentStatus::Submitted,
        vec![allocation(&bill, 1)],
    );
    let result = reconcile(
        &snapshot(vec![
            notice("notice", &bill, BillKind::Annual),
            posted,
            submitted,
        ]),
        now(),
    );
    assert_eq!(
        installment(&result, &bill, 1),
        InstallmentState::Contradiction
    );
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "payment_conflict")
    );
    let unallocated = observation(
        "unallocated",
        ObservationData::Payment(Payment {
            reference: "FICTIONAL-UNALLOCATED".to_owned(),
            status: PaymentStatus::CountyPosted,
            amount_cents: 25_000,
            allocations: vec![],
        }),
        now(),
    );
    let result = reconcile(
        &snapshot(vec![notice("notice", &bill, BillKind::Annual), unallocated]),
        now(),
    );
    assert_eq!(installment(&result, &bill, 1), InstallmentState::Open);
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "unallocated_payment")
    );
}

#[test]
fn one_complete_capability_is_not_complete_county_coverage() {
    let mut input = snapshot(vec![]);
    input.coverage = vec![Coverage {
        capability: CoverageCapability::Notices,
        status: CoverageStatus::Complete,
        checked_at: now(),
        detail: "Fictional notices check only".to_owned(),
    }];
    let result = reconcile(&input, now());
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "coverage_incomplete")
    );
    assert_eq!(result.known_balance_cents, None);
}

#[test]
fn adjusted_bill_supersedes_only_its_exact_predecessor() {
    let original = identity("annual", "original");
    let adjusted = identity("annual", "adjusted");
    let supplemental = identity("supplemental", "original");
    let mut replacement = notice("adjusted", &adjusted, BillKind::Annual);
    if let ObservationData::Notice(notice) = &mut replacement.observation {
        notice.supersedes = Some(original.clone());
    }
    let result = reconcile(
        &snapshot(vec![
            notice("original", &original, BillKind::Annual),
            replacement,
            notice("supplemental", &supplemental, BillKind::Supplemental),
            payment(
                "old-posting",
                PaymentStatus::CountyPosted,
                vec![allocation(&original, 1)],
            ),
        ]),
        now(),
    );
    assert!(
        result
            .bills
            .iter()
            .find(|bill| bill.identity == original)
            .unwrap()
            .superseded
    );
    assert!(
        result
            .installments
            .iter()
            .all(|item| item.identity != original)
    );
    assert_eq!(installment(&result, &adjusted, 1), InstallmentState::Open);
    assert_eq!(
        installment(&result, &supplemental, 1),
        InstallmentState::Open
    );
}

#[test]
fn a_conflicting_replacement_cannot_hide_the_original_bill() {
    let original = identity("annual", "original");
    let mut incorrect = notice(
        "wrong-kind-replacement",
        &identity("annual", "adjusted"),
        BillKind::Supplemental,
    );
    if let ObservationData::Notice(notice) = &mut incorrect.observation {
        notice.supersedes = Some(original.clone());
    }
    let result = reconcile(
        &snapshot(vec![
            notice("original", &original, BillKind::Annual),
            incorrect,
        ]),
        now(),
    );
    assert!(
        !result
            .bills
            .iter()
            .find(|bill| bill.identity == original)
            .unwrap()
            .superseded
    );
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "conflicting_supersession")
    );
}

#[test]
fn duplicate_notices_with_different_amounts_are_not_silently_selected() {
    let bill = identity("annual", "original");
    let first = notice("first", &bill, BillKind::Annual);
    let mut changed = first.clone();
    changed.id = "second".to_owned();
    if let ObservationData::Notice(notice) = &mut changed.observation {
        notice.installments[0].amount_cents += 1;
    }
    let result = reconcile(&snapshot(vec![first, changed]), now());
    assert_eq!(
        installment(&result, &bill, 1),
        InstallmentState::Contradiction
    );
    assert!(
        result
            .issues
            .iter()
            .any(|issue| issue.code == "notice_conflict")
    );
}

#[test]
fn delinquency_uses_the_supplied_instant_not_utc_calendar_midnight() {
    let bill = identity("annual", "original");
    let input = snapshot(vec![notice("notice", &bill, BillKind::Annual)]);
    let before = reconcile(&input, "2030-12-11T00:59:59Z".parse().unwrap());
    let boundary = reconcile(&input, "2030-12-11T01:00:00Z".parse().unwrap());
    assert_eq!(installment(&before, &bill, 1), InstallmentState::Open);
    assert_eq!(
        installment(&boundary, &bill, 1),
        InstallmentState::Delinquent
    );
}

#[test]
fn malformed_records_wrong_binding_and_bank_claimed_postings_are_rejected() {
    let bill = identity("annual", "original");
    let mut input = import(vec![notice("notice", &bill, BillKind::Annual)]);
    input.observations[0].binding.parcel =
        ParcelId::try_from("FICTIONAL-OTHER-PARCEL".to_owned()).unwrap();
    assert!(matches!(input.validate(), Err(TaxError::BindingMismatch)));
    let mut claimed = payment(
        "bank-claimed-posting",
        PaymentStatus::CountyPosted,
        vec![allocation(&bill, 1)],
    );
    claimed.evidence.source = SourceKind::Bank;
    assert!(matches!(
        import(vec![claimed]).validate(),
        Err(TaxError::Invalid(_))
    ));
    let mut malformed = notice("bad-ocr", &bill, BillKind::Annual);
    if let ObservationData::Notice(notice) = &mut malformed.observation {
        notice.installments[0].amount_cents = -10;
    }
    assert!(matches!(
        import(vec![malformed]).validate(),
        Err(TaxError::Invalid(_))
    ));
    assert!(CountyFips::try_from("not-a-county".to_owned()).is_err());
    assert!(serde_json::from_str::<ImportInput>(r#"{"property":"malformed OCR text"}"#).is_err());
}

#[test]
fn money_must_survive_json_round_trips_without_losing_a_cent() {
    let bill = identity("annual", "original");
    let too_large = 9_007_199_254_740_992_i64;
    let bad_amounts = vec![
        ObservationData::Balance(Balance {
            scope: BalanceScope::Property,
            amount_cents: Some(too_large),
        }),
        ObservationData::Assessment(Assessment {
            fiscal_year: 2030,
            assessed_value_cents: too_large,
            description: "Fictional assessment".to_owned(),
        }),
        ObservationData::Payment(Payment {
            reference: "Fictional payment".to_owned(),
            status: PaymentStatus::Submitted,
            amount_cents: too_large,
            allocations: vec![],
        }),
    ];
    for (index, data) in bad_amounts.into_iter().enumerate() {
        assert!(matches!(
            import(vec![observation(
                &format!("too-large-{index}"),
                data,
                now()
            )])
            .validate(),
            Err(TaxError::Invalid(_))
        ));
    }
    let mut notice = notice("unsafe-notice", &bill, BillKind::Annual);
    if let ObservationData::Notice(notice) = &mut notice.observation {
        notice.installments[0].amount_cents = too_large;
    }
    assert!(matches!(
        import(vec![notice]).validate(),
        Err(TaxError::Invalid(_))
    ));
}

#[test]
fn notice_deadline_validation_uses_the_declared_local_day() {
    let mut data = notice("notice", &identity("annual", "original"), BillKind::Annual);
    if let ObservationData::Notice(notice) = &mut data.observation {
        notice.installments[0].due_date = NaiveDate::from_ymd_opt(2030, 11, 1).unwrap();
        // This is still October 31 in the declared county timezone.
        notice.installments[0].delinquent_at = "2030-11-01T01:00:00Z".parse().unwrap();
    }
    assert!(matches!(
        import(vec![data]).validate(),
        Err(TaxError::Invalid(_))
    ));
}

#[tokio::test]
async fn preview_apply_duplicates_and_receipts_are_durable_after_cold_restore() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("ledger.sqlite3");
    let store = Store::open(&path).await.unwrap();
    let item = notice("notice", &identity("annual", "original"), BillKind::Annual);
    let preview = store
        .preview(import(vec![item.clone(), item.clone()]))
        .await
        .unwrap();
    assert_eq!((preview.added, preview.duplicates), (1, 1));
    assert!(preview.duplicate_ids.is_empty());
    let request = ApplyRequest {
        if_match: preview.revision,
        preview,
        idempotency_key: "fictional-operation".to_owned(),
    };
    let first = store.apply(request.clone()).await.unwrap();
    assert_eq!(first.revision, 1);
    assert!(!first.replayed);
    let saved = store.snapshot().await.unwrap();
    assert_eq!(saved.observations, vec![item]);
    let repeated = store
        .preview(import(saved.observations.clone()))
        .await
        .unwrap();
    assert_eq!((repeated.added, repeated.duplicates), (0, 1));
    assert_eq!(repeated.duplicate_ids, vec!["notice".to_owned()]);
    store.close().await.unwrap();
    let restored_path = temp.path().join("restored.sqlite3");
    std::fs::copy(&path, &restored_path).unwrap();
    let restored = Store::open(&restored_path).await.unwrap();
    assert_eq!(restored.snapshot().await.unwrap(), saved);
    assert!(restored.apply(request).await.unwrap().replayed);
    assert_eq!(restored.snapshot().await.unwrap().revision, 1);
    restored.close().await.unwrap();
}

#[tokio::test]
async fn changed_preview_wrong_revision_and_conflicting_ids_change_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let input = import(vec![notice(
        "notice",
        &identity("annual", "original"),
        BillKind::Annual,
    )]);
    let preview = store.preview(input.clone()).await.unwrap();
    let mut tampered = preview.clone();
    tampered.input.property.label = "Changed input".to_owned();
    assert!(matches!(
        store
            .apply(ApplyRequest {
                preview: tampered,
                if_match: 0,
                idempotency_key: "tampered".to_owned()
            })
            .await,
        Err(TaxError::PreviewMismatch)
    ));
    assert_eq!(store.snapshot().await.unwrap().revision, 0);
    let request = ApplyRequest {
        preview,
        if_match: 0,
        idempotency_key: "one".to_owned(),
    };
    store.apply(request.clone()).await.unwrap();
    let mut incorrect_replay = request;
    incorrect_replay.if_match = 99;
    assert!(matches!(
        store.apply(incorrect_replay).await,
        Err(TaxError::RevisionConflict)
    ));
    let mut conflicting = input.clone();
    conflicting.observations[0].evidence.document = "Different evidence under same id".to_owned();
    assert!(matches!(
        store.preview(conflicting).await,
        Err(TaxError::ObservationConflict)
    ));
    let mut other_property = input;
    other_property.property.key.parcel =
        ParcelId::try_from("FICTIONAL-OTHER-PARCEL".to_owned()).unwrap();
    other_property.observations.clear();
    assert!(matches!(
        store.preview(other_property).await,
        Err(TaxError::PropertyChanged)
    ));
    assert_eq!(store.snapshot().await.unwrap().revision, 1);
}

#[tokio::test]
async fn concurrent_previews_cannot_overwrite_a_committed_import() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let first = store
        .preview(import(vec![notice(
            "annual",
            &identity("annual", "original"),
            BillKind::Annual,
        )]))
        .await
        .unwrap();
    let second = store
        .preview(import(vec![notice(
            "supplemental",
            &identity("supplemental", "original"),
            BillKind::Supplemental,
        )]))
        .await
        .unwrap();
    let a = ApplyRequest {
        preview: first,
        if_match: 0,
        idempotency_key: "first".to_owned(),
    };
    let b = ApplyRequest {
        preview: second,
        if_match: 0,
        idempotency_key: "second".to_owned(),
    };
    let (a, b) = tokio::join!(store.apply(a), store.apply(b));
    assert!(matches!(
        (&a, &b),
        (Ok(_), Err(TaxError::RevisionConflict)) | (Err(TaxError::RevisionConflict), Ok(_))
    ));
    let result = store.snapshot().await.unwrap();
    assert_eq!(result.revision, 1);
    assert_eq!(result.observations.len(), 1);
}

#[tokio::test]
async fn idempotency_keys_cannot_be_reused_for_a_different_import() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let preview = store.preview(import(vec![])).await.unwrap();
    store
        .apply(ApplyRequest {
            preview,
            if_match: 0,
            idempotency_key: "same-key".to_owned(),
        })
        .await
        .unwrap();
    let preview = store
        .preview(import(vec![notice(
            "notice",
            &identity("annual", "original"),
            BillKind::Annual,
        )]))
        .await
        .unwrap();
    assert!(matches!(
        store
            .apply(ApplyRequest {
                preview,
                if_match: 1,
                idempotency_key: "same-key".to_owned()
            })
            .await,
        Err(TaxError::IdempotencyConflict)
    ));
    assert_eq!(store.snapshot().await.unwrap().revision, 1);
    assert!(store.snapshot().await.unwrap().observations.is_empty());
}

#[tokio::test]
async fn sync_coverage_updates_preserve_evidence_and_conflict_with_stale_previews() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let input = import(vec![observation(
        "assessment",
        ObservationData::Assessment(Assessment {
            fiscal_year: 2030,
            assessed_value_cents: 10_000_000,
            description: "Fictional assessment".to_owned(),
        }),
        now(),
    )]);
    let preview = store.preview(input).await.unwrap();
    store
        .apply(ApplyRequest {
            preview,
            if_match: 0,
            idempotency_key: "initial".to_owned(),
        })
        .await
        .unwrap();
    let saved = store.snapshot().await.unwrap();
    let pending = store.preview(import(vec![])).await.unwrap();
    let coverage = Coverage {
        capability: CoverageCapability::Balances,
        status: CoverageStatus::Unavailable,
        checked_at: now(),
        detail: "Fictional county outage".to_owned(),
    };
    assert_eq!(
        store
            .record_coverage(vec![coverage.clone()], 1)
            .await
            .unwrap(),
        2
    );
    assert!(matches!(
        store
            .apply(ApplyRequest {
                preview: pending,
                if_match: 1,
                idempotency_key: "stale".to_owned()
            })
            .await,
        Err(TaxError::RevisionConflict)
    ));
    let result = store.snapshot().await.unwrap();
    assert_eq!(result.observations, saved.observations);
    assert_eq!(result.coverage, vec![coverage]);
    assert_eq!(reconcile(&result, now()).known_balance_cents, None);
}

#[tokio::test]
async fn observation_pages_filter_kind_seek_without_gaps_and_reject_a_changed_revision() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let bill = identity("annual", "original");
    let mut observations = vec![notice("unrelated-notice", &bill, BillKind::Annual)];
    let mut expected_payments = Vec::new();
    let mut expected_assessments = Vec::new();
    // Deliberately reverse insertion order and interleave another kind, so the
    // persisted kind/id keyset defines page order rather than input order.
    for number in (0..22).rev() {
        let id = format!("fictional-row-{number:02}");
        if number % 2 == 0 {
            expected_assessments.push(id.clone());
            observations.push(observation(
                &id,
                ObservationData::Assessment(Assessment {
                    fiscal_year: 2030,
                    assessed_value_cents: 1_000_000,
                    description: "Fictional assessment".to_owned(),
                }),
                now(),
            ));
        } else {
            expected_payments.push(id.clone());
            observations.push(payment(
                &id,
                PaymentStatus::Submitted,
                vec![allocation(&bill, 1)],
            ));
        }
    }
    expected_payments.sort();
    expected_assessments.sort();
    let preview = store.preview(import(observations)).await.unwrap();
    store
        .apply(ApplyRequest {
            preview,
            if_match: 0,
            idempotency_key: "fictional-page-import".to_owned(),
        })
        .await
        .unwrap();
    let mut payment_ids = Vec::new();
    let mut after = None;
    loop {
        let page = store
            .observation_page(ObservationKind::Payment, after.as_deref(), 3, Some(1))
            .await
            .unwrap();
        assert_eq!(page.revision, 1);
        assert!(page.items.len() <= 3);
        if page.items.is_empty() {
            break;
        }
        assert!(
            page.items
                .iter()
                .all(|item| matches!(item.observation, ObservationData::Payment(_)))
        );
        after = page.items.last().map(|item| item.id.clone());
        payment_ids.extend(page.items.into_iter().map(|item| item.id));
    }
    assert_eq!(payment_ids, expected_payments);
    let assessments = store
        .observation_page(ObservationKind::Assessment, None, 201, None)
        .await
        .unwrap();
    assert_eq!(assessments.revision, 1);
    assert_eq!(
        assessments
            .items
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        expected_assessments
    );
    assert!(
        store
            .observation_page(ObservationKind::Payment, Some("zzzz"), 201, Some(1))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    for limit in [0, 202] {
        assert!(matches!(
            store
                .observation_page(ObservationKind::Payment, None, limit, None)
                .await,
            Err(TaxError::Invalid(_))
        ));
    }
    let preview = store
        .preview(import(vec![observation(
            "new-assessment",
            ObservationData::Assessment(Assessment {
                fiscal_year: 2031,
                assessed_value_cents: 1_000_000,
                description: "Fictional update".to_owned(),
            }),
            now(),
        )]))
        .await
        .unwrap();
    store
        .apply(ApplyRequest {
            preview,
            if_match: 1,
            idempotency_key: "fictional-page-update".to_owned(),
        })
        .await
        .unwrap();
    assert!(matches!(
        store
            .observation_page(ObservationKind::Payment, Some(&payment_ids[2]), 3, Some(1))
            .await,
        Err(TaxError::RevisionConflict)
    ));
    let refreshed = store
        .observation_page(ObservationKind::Payment, None, 201, None)
        .await
        .unwrap();
    assert_eq!(refreshed.revision, 2);
    assert_eq!(
        refreshed
            .items
            .into_iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        expected_payments
    );
}

#[tokio::test]
async fn oversized_projections_are_rejected_before_preview_and_smaller_previews_stay_applyable() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("ledger.sqlite3"))
        .await
        .unwrap();
    let observations = (0..1000)
        .map(|index| {
            let bill = identity(&format!("fictional-bill-{index:04}"), "original");
            let mut item = notice(
                &format!("fictional-notice-{index:04}"),
                &bill,
                BillKind::Annual,
            );
            if let ObservationData::Notice(notice) = &mut item.observation {
                let template = notice.installments[0].clone();
                notice.installments = (1..=12)
                    .map(|number| Installment {
                        number,
                        ..template.clone()
                    })
                    .collect();
            }
            item
        })
        .collect();
    let input = import(observations);
    let input_bytes = serde_json::to_vec(&input).unwrap().len();
    assert!(input_bytes > 1_000_000);
    assert!(input_bytes <= property::tax::ledger::MAX_IMPORT_BYTES);
    input.validate().unwrap();
    assert!(matches!(
        store.preview(input.clone()).await,
        Err(TaxError::Capacity)
    ));
    assert_eq!(store.snapshot().await.unwrap().revision, 0);
    assert!(store.snapshot().await.unwrap().observations.is_empty());
    let mut smaller = input.clone();
    smaller.observations.truncate(300);
    for observation in &mut smaller.observations {
        if let ObservationData::Notice(notice) = &mut observation.observation {
            notice.installments.truncate(2);
        }
    }
    let preview = store.preview(smaller).await.unwrap();
    assert_eq!(preview.summary.bill_count, 300);
    assert_eq!(preview.summary.installment_count, 600);
    let preview_bytes = serde_json::to_vec(&preview).unwrap().len();
    assert!(preview_bytes < 8 * 1024 * 1024);
    let request = ApplyRequest {
        preview,
        if_match: 0,
        idempotency_key: "fictional-bulk-operation".to_owned(),
    };
    assert!(serde_json::to_vec(&request).unwrap().len() < 8 * 1024 * 1024);
    assert_eq!(store.apply(request).await.unwrap().added, 300);
    assert_eq!(store.snapshot().await.unwrap().observations.len(), 300);
    let mut oversized = input;
    for observation in &mut oversized.observations {
        observation.evidence.url = Some(format!("https://example.invalid/{}", "x".repeat(1970)));
        observation.evidence.document = "fictional".repeat(55);
    }
    assert!(
        serde_json::to_vec(&oversized).unwrap().len() > property::tax::ledger::MAX_IMPORT_BYTES
    );
    assert!(matches!(oversized.validate(), Err(TaxError::Capacity)));
}

#[test]
fn separately_identified_county_return_cannot_leave_prior_credit_paid() {
    let bill = identity("000", "unversioned");
    let posted_at = now() - Duration::days(2);
    let returned_at = now() - Duration::days(1);
    let mut posted = payment(
        "county-posted",
        PaymentStatus::CountyPosted,
        vec![allocation(&bill, 1)],
    );
    posted.evidence = evidence(SourceKind::County, "fictional posted record", posted_at);
    if let ObservationData::Payment(value) = &mut posted.observation {
        value.reference = "FICTIONAL-COUNTY-ID-101".into();
    }
    let mut returned = payment(
        "county-returned",
        PaymentStatus::Refunded,
        vec![allocation(&bill, 1)],
    );
    returned.evidence = evidence(
        SourceKind::County,
        "fictional separate refund record",
        returned_at,
    );
    if let ObservationData::Payment(value) = &mut returned.observation {
        value.reference = "FICTIONAL-COUNTY-ID-102".into();
    }
    let mut snapshot = Snapshot {
        revision: 1,
        property: Some(binding()),
        coverage: Vec::new(),
        observations: vec![notice("annual", &bill, BillKind::Annual), posted, returned],
    };
    let result = reconcile(&snapshot, now());
    assert_eq!(
        result
            .installments
            .iter()
            .find(|item| item.number == 1)
            .unwrap()
            .state,
        InstallmentState::Contradiction
    );
    assert_ne!(
        result
            .installments
            .iter()
            .find(|item| item.number == 2)
            .unwrap()
            .state,
        InstallmentState::Paid
    );
    snapshot.observations.push(balance(
        "later-county-zero",
        BalanceScope::Installment { bill, number: 1 },
        Some(0),
        now(),
    ));
    let result = reconcile(&snapshot, now());
    assert_eq!(
        result
            .installments
            .iter()
            .find(|item| item.number == 1)
            .unwrap()
            .state,
        InstallmentState::Paid
    );
}
