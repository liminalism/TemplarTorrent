use templartorrent_core::budget::ByteBudget;

#[test]
fn lowering_budget_keeps_all_existing_owners_charged_until_they_release() {
    let mut budget = ByteBudget::new(1000);
    let alias = budget.clone();
    let first = alias.try_acquire(600).unwrap();
    let second = budget.try_acquire(200).unwrap();
    budget.set_limit(500);
    assert_eq!(alias.limit(), 500);
    assert_eq!(budget.used(), 800);
    assert!(alias.try_acquire(1).is_none());
    drop(first);
    assert_eq!(budget.used(), 200);
    let third = alias.try_acquire(300).unwrap();
    assert!(budget.try_acquire(1).is_none());
    budget.set_limit(1000);
    assert_eq!(
        budget.used(),
        500,
        "settings cannot mint uncharged capacity"
    );
    drop(second);
    drop(third);
    assert_eq!(budget.used(), 0);
    assert_eq!(budget.high_watermark(), 800);
}
