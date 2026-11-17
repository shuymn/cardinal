//! Budget math (`docs/architecture.md` section 4.4): pure functions, no I/O.

use std::num::NonZeroU64;

use crate::contract::Resources;

/// The per-resource sum of `reservations`.
pub fn sum<'a>(reservations: impl IntoIterator<Item = &'a Resources>) -> Resources {
    let mut total = Resources::new();
    for resources in reservations {
        for (name, amount) in resources {
            total
                .entry(name.clone())
                .and_modify(|sum| *sum = sum.saturating_add(amount.get()))
                .or_insert(*amount);
        }
    }
    total
}

/// Whether `need` fits next to `reserved` within `budget`: for every resource the job needs,
/// `reserved + need ≤ budget`. A resource without a budget always fits. All or nothing: one
/// resource that does not fit means the job does not fit.
#[must_use]
pub fn fits(budget: &Resources, reserved: &Resources, need: &Resources) -> bool {
    need.iter().all(|(name, amount)| {
        let Some(limit) = budget.get(name) else {
            return true;
        };
        let used = reserved.get(name).map_or(0, |n| n.get());
        used.checked_add(amount.get())
            .is_some_and(|total| total <= limit.get())
    })
}

/// A resource map from pairs, for tests and callers that build one by hand.
#[must_use]
pub fn resources<'a>(pairs: impl IntoIterator<Item = (&'a str, u64)>) -> Resources {
    pairs
        .into_iter()
        .filter_map(|(name, amount)| Some((name.to_owned(), NonZeroU64::new(amount)?)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{fits, resources, sum};

    #[test]
    fn sums_per_resource() {
        let a = resources([("cpu", 2), ("memory_mb", 100)]);
        let b = resources([("cpu", 3), ("gpu", 1)]);
        assert_eq!(
            sum([&a, &b]),
            resources([("cpu", 5), ("memory_mb", 100), ("gpu", 1)])
        );
        assert_eq!(sum([]), resources([]));
    }

    #[test]
    fn fits_only_when_every_needed_resource_fits() {
        let budget = resources([("cpu", 8), ("memory_mb", 1000)]);
        let reserved = resources([("cpu", 6), ("memory_mb", 1000)]);
        assert!(fits(&budget, &reserved, &resources([("cpu", 2)])));
        assert!(!fits(&budget, &reserved, &resources([("cpu", 3)])));
        assert!(
            !fits(
                &budget,
                &reserved,
                &resources([("cpu", 1), ("memory_mb", 1)])
            ),
            "the job gets all of its resources or none"
        );
        assert!(
            fits(&budget, &reserved, &resources([("cpu", 1), ("gpu", 99)])),
            "a resource without a budget is unlimited"
        );
        assert!(fits(&budget, &reserved, &resources([])));
        assert!(
            !fits(&budget, &resources([]), &resources([("cpu", 9)])),
            "a job larger than the budget never fits"
        );
    }

    #[test]
    fn overflowing_sums_do_not_fit() {
        let budget = resources([("cpu", u64::MAX)]);
        let reserved = resources([("cpu", u64::MAX)]);
        assert!(!fits(&budget, &reserved, &resources([("cpu", 1)])));
        assert_eq!(sum([&reserved, &reserved]), resources([("cpu", u64::MAX)]));
    }
}
