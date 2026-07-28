//! Best-effort journey fare estimation from GTFS `fare_attributes` / `fare_rules`.
//!
//! # Limitations
//!
//! - **Not ticket purchase**: amounts are rough estimates for display only.
//! - French / national feeds often omit or only partially publish fare tables.
//! - Transfer products, time-based validity, and multi-operator pass logic are not modeled.
//! - Prices are never invented: no match or empty tables ⇒ `None`.
//!
//! # Matching strategy
//!
//! 1. Collect transit legs with namespaced `route_id` and board/alight stop `zone_id`
//!    (from [`crate::gtfs::pack::StopRecord::zone_id`], namespaced as `feed:zone` when present).
//! 2. Collect the set of all zones visited by transit legs (board + alight).
//! 3. A rule **matches** when all of its set filters pass:
//!    - `route_id`: empty **or** equals a leg route;
//!    - `origin_id`: empty **or** equals the **board** stop `zone_id` of some matching leg
//!      (or of a leg that also satisfies route when route is set);
//!    - `destination_id`: empty **or** equals the **alight** stop `zone_id` of that same leg;
//!    - `contains_id`: empty **or** present in the journey zone set.
//! 4. **Specificity** (higher wins when several rules match the same journey):
//!    - +4 if `route_id` is set
//!    - +2 if `origin_id` is set
//!    - +2 if `destination_id` is set
//!    - +1 if `contains_id` is set
//!    So **route+zones > route-only > network-wide**. Only rules at the maximum
//!    specificity among matches are kept (avoids double-counting a flat network fare
//!    when a zone product already applies).
//! 5. Collect unique matched `FareAttribute`s. If all share one currency, **sum** their
//!    prices. Mixed currencies or empty match set ⇒ `None`.

use std::collections::{HashMap, HashSet};

use crate::gtfs::pack::{FareAttribute, FareRule, StaticEpoch};

use super::journey::{Journey, Leg};

/// Estimated fare for a planned journey.
#[derive(Debug, Clone, PartialEq)]
pub struct FareEstimate {
    pub amount: f64,
    pub currency: String,
    pub fare_ids: Vec<String>,
    pub note: String,
}

/// One transit leg with route + zone context for fare matching.
struct LegFareCtx<'a> {
    route_id: &'a str,
    origin_zone: Option<&'a str>,
    dest_zone: Option<&'a str>,
}

fn rule_specificity(rule: &FareRule) -> u8 {
    let mut s = 0u8;
    if rule.route_id.is_some() {
        s += 4;
    }
    if rule.origin_id.is_some() {
        s += 2;
    }
    if rule.destination_id.is_some() {
        s += 2;
    }
    if rule.contains_id.is_some() {
        s += 1;
    }
    s
}

/// Whether `rule` matches at least one transit leg under the given zone set.
fn rule_matches_legs(rule: &FareRule, legs: &[LegFareCtx<'_>], journey_zones: &HashSet<&str>) -> bool {
    if let Some(ref contains) = rule.contains_id {
        if !journey_zones.contains(contains.as_str()) {
            return false;
        }
    }

    for leg in legs {
        if let Some(ref rid) = rule.route_id {
            if rid.as_str() != leg.route_id {
                continue;
            }
        }
        if let Some(ref oid) = rule.origin_id {
            if leg.origin_zone != Some(oid.as_str()) {
                continue;
            }
        }
        if let Some(ref did) = rule.destination_id {
            if leg.dest_zone != Some(did.as_str()) {
                continue;
            }
        }
        return true;
    }
    false
}

/// Estimate journey fare from epoch fare tables. Returns `None` if no data or no match.
pub fn estimate_journey_fare(epoch: &StaticEpoch, journey: &Journey) -> Option<FareEstimate> {
    if epoch.fares.is_empty() {
        return None;
    }

    let attr_by_id: HashMap<&str, &FareAttribute> = epoch
        .fares
        .iter()
        .map(|f| (f.fare_id.as_str(), f))
        .collect();

    let mut leg_ctxs: Vec<LegFareCtx<'_>> = Vec::new();
    let mut journey_zones: HashSet<&str> = HashSet::new();

    for leg in &journey.legs {
        let Leg::Transit(t) = leg else { continue };
        let Some(trip) = epoch
            .trip_id_to_idx
            .get(&t.trip_id)
            .and_then(|&i| epoch.trips.get(i as usize))
        else {
            continue;
        };
        let origin_zone = epoch
            .stop_id_to_idx
            .get(&t.from.stop_id)
            .and_then(|&i| epoch.stops.get(i as usize))
            .and_then(|s| s.zone_id.as_deref());
        let dest_zone = epoch
            .stop_id_to_idx
            .get(&t.to.stop_id)
            .and_then(|&i| epoch.stops.get(i as usize))
            .and_then(|s| s.zone_id.as_deref());
        if let Some(z) = origin_zone {
            journey_zones.insert(z);
        }
        if let Some(z) = dest_zone {
            journey_zones.insert(z);
        }
        leg_ctxs.push(LegFareCtx {
            route_id: trip.route_id.as_str(),
            origin_zone,
            dest_zone,
        });
    }
    if leg_ctxs.is_empty() {
        return None;
    }

    // If there are no rules, fares alone are not applied (GTFS: attributes without rules
    // may be agency-wide, but without rules we cannot safely assign them to a journey).
    if epoch.fare_rules.is_empty() {
        return None;
    }

    let mut matched: Vec<(&FareRule, u8)> = Vec::new();
    for rule in &epoch.fare_rules {
        if !attr_by_id.contains_key(rule.fare_id.as_str()) {
            continue;
        }
        if rule_matches_legs(rule, &leg_ctxs, &journey_zones) {
            matched.push((rule, rule_specificity(rule)));
        }
    }

    if matched.is_empty() {
        return None;
    }

    let max_spec = matched.iter().map(|(_, s)| *s).max().unwrap_or(0);
    let mut matched_ids: HashSet<String> = HashSet::new();
    for (rule, spec) in matched {
        if spec == max_spec {
            matched_ids.insert(rule.fare_id.clone());
        }
    }

    let mut attrs: Vec<&FareAttribute> = matched_ids
        .iter()
        .filter_map(|id| attr_by_id.get(id.as_str()).copied())
        .collect();
    attrs.sort_by(|a, b| a.fare_id.cmp(&b.fare_id));

    if attrs.is_empty() {
        return None;
    }

    let currency = attrs[0].currency_type.clone();
    if attrs.iter().any(|f| f.currency_type != currency) {
        return None;
    }

    let amount: f64 = attrs.iter().map(|f| f.price).sum();
    let fare_ids: Vec<String> = attrs.iter().map(|f| f.fare_id.clone()).collect();

    let note = if attrs.len() == 1 {
        "Best-effort GTFS fare estimate (single product). Not for purchase; data may be incomplete."
            .to_string()
    } else {
        "Best-effort GTFS fare estimate (sum of unique matched products). \
         Transfer tickets may be cheaper; data may be incomplete. Not for purchase."
            .to_string()
    };

    Some(FareEstimate {
        amount,
        currency,
        fare_ids,
        note,
    })
}

/// Apply [`estimate_journey_fare`] onto journey fare fields.
pub fn apply_fare_estimate(epoch: &StaticEpoch, journey: &mut Journey) {
    if let Some(est) = estimate_journey_fare(epoch, journey) {
        journey.fare_amount = Some(est.amount);
        journey.fare_currency = Some(est.currency);
        journey.fare_note = Some(est.note);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gtfs::pack::{
        FareAttribute, FareRule, FrequencyWindow, GlobalTrip, PackedStopTime, RouteMode,
        StaticEpoch, StopRecord,
    };
    use crate::routing::journey::{
        new_journey_id, IntermediateStop, Journey, Leg, StopRef, TransitLegData, WalkLegData,
    };
    use chrono::{TimeZone, Utc};

    fn stop(id: &str, idx_hint: &str) -> StopRecord {
        StopRecord {
            id: id.into(),
            feed_id: "test".into(),
            raw_id: idx_hint.into(),
            name: id.into(),
            lat: Some(48.0),
            lon: Some(2.0),
            parent_id: None,
            location_type: 0,
            platform_code: None,
            wheelchair: 0,
            stop_code: None,
            stop_desc: None,
            level_id: None,
            zone_id: None,
            stop_url: None,
            stop_timezone: None,
        }
    }

    fn stop_zoned(id: &str, idx_hint: &str, zone: &str) -> StopRecord {
        let mut s = stop(id, idx_hint);
        s.zone_id = Some(zone.into());
        s
    }

    fn base_epoch() -> StaticEpoch {
        let mut epoch = StaticEpoch::empty();
        epoch.stops = vec![stop("test:A", "A"), stop("test:B", "B")];
        epoch.stop_id_to_idx.insert("test:A".into(), 0);
        epoch.stop_id_to_idx.insert("test:B".into(), 1);
        epoch.stop_times = vec![
            PackedStopTime {
                stop_idx: 0,
                arrival_s: 8 * 3600,
                departure_s: 8 * 3600,
                stop_sequence: 1,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
            PackedStopTime {
                stop_idx: 1,
                arrival_s: 9 * 3600,
                departure_s: 9 * 3600,
                stop_sequence: 2,
                pickup_type: 0,
                drop_off_type: 0,
                stop_headsign_idx: 0,
                timepoint: 1,
                shape_dist_traveled: None,
            },
        ];
        epoch.trips.push(GlobalTrip {
            id: "test:t1".into(),
            feed_id: "test".into(),
            route_id: "test:r1".into(),
            service_id: "test:S1".into(),
            headsign: None,
            short_name: None,
            direction_id: None,
            wheelchair: 0,
            bikes_allowed: 0,
            block_id: None,
            shape_id: None,
            mode: RouteMode::Bus,
            route_short_name: "1".into(),
            route_long_name: "Line 1".into(),
            route_color: None,
            route_text_color: None,
            route_type_raw: 3,
            agency_name: None,
            stop_time_start: 0,
            stop_time_len: 2,
            frequency_windows: Vec::<FrequencyWindow>::new(),
        });
        epoch.trip_id_to_idx.insert("test:t1".into(), 0);
        epoch
    }

    fn sample_journey() -> Journey {
        let t0 = Utc.with_ymd_and_hms(2026, 6, 1, 8, 0, 0).unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 6, 1, 9, 0, 0).unwrap();
        Journey {
            id: new_journey_id(),
            departure: t0,
            arrival: t1,
            duration_s: 3600,
            transfers: 0,
            walk_distance_m: 0.0,
            realtime_status: "SCHEDULED".into(),
            legs: vec![Leg::Transit(TransitLegData {
                mode: "BUS".into(),
                route_short_name: "1".into(),
                route_long_name: "Line 1".into(),
                route_id: "test:r1".into(),
                agency_name: None,
                trip_id: "test:t1".into(),
                trip_short_name: None,
                headsign: None,
                direction_id: None,
                route_color: None,
                route_text_color: None,
                stop_headsign: None,
                wheelchair: 0,
                bikes_allowed: 0,
                from: StopRef {
                    stop_id: "test:A".into(),
                    name: "A".into(),
                    lat: Some(48.0),
                    lon: Some(2.0),
                    platform: None,
                },
                to: StopRef {
                    stop_id: "test:B".into(),
                    name: "B".into(),
                    lat: Some(48.1),
                    lon: Some(2.1),
                    platform: None,
                },
                from_stop_sequence: 1,
                to_stop_sequence: 2,
                scheduled_departure: t0,
                scheduled_arrival: t1,
                realtime_departure: None,
                realtime_arrival: None,
                delay_departure_s: None,
                delay_arrival_s: None,
                canceled: false,
                vehicle_lat: None,
                vehicle_lon: None,
                vehicle_updated_at: None,
                intermediate_stops: Vec::<IntermediateStop>::new(),
                geometry: vec![],
                same_vehicle: false,
            })],
            alert_headers: vec![],
            fare_amount: None,
            fare_currency: None,
            fare_note: None,
        }
    }

    #[test]
    fn no_fares_returns_none() {
        let epoch = base_epoch();
        let j = sample_journey();
        assert!(estimate_journey_fare(&epoch, &j).is_none());
    }

    #[test]
    fn route_matched_fare() {
        let mut epoch = base_epoch();
        epoch.fares.push(FareAttribute {
            fare_id: "test:flat".into(),
            price: 2.5,
            currency_type: "EUR".into(),
            payment_method: 1,
            transfers: Some(0),
            transfer_duration: None,
        });
        epoch.fare_rules.push(FareRule {
            fare_id: "test:flat".into(),
            route_id: Some("test:r1".into()),
            origin_id: None,
            destination_id: None,
            contains_id: None,
        });
        let j = sample_journey();
        let est = estimate_journey_fare(&epoch, &j).expect("fare");
        assert!((est.amount - 2.5).abs() < 1e-9);
        assert_eq!(est.currency, "EUR");
        assert_eq!(est.fare_ids, vec!["test:flat".to_string()]);
        assert!(!est.note.is_empty());
    }

    #[test]
    fn zone_rule_no_match_without_stop_zones() {
        let mut epoch = base_epoch();
        epoch.fares.push(FareAttribute {
            fare_id: "test:z".into(),
            price: 9.0,
            currency_type: "EUR".into(),
            payment_method: 1,
            transfers: None,
            transfer_duration: None,
        });
        epoch.fare_rules.push(FareRule {
            fare_id: "test:z".into(),
            route_id: Some("test:r1".into()),
            origin_id: Some("test:zoneA".into()),
            destination_id: Some("test:zoneB".into()),
            contains_id: None,
        });
        let j = sample_journey();
        assert!(estimate_journey_fare(&epoch, &j).is_none());
    }

    #[test]
    fn zone_od_rule_matches_stop_zones() {
        let mut epoch = base_epoch();
        epoch.stops = vec![
            stop_zoned("test:A", "A", "test:zoneA"),
            stop_zoned("test:B", "B", "test:zoneB"),
        ];
        epoch.fares.push(FareAttribute {
            fare_id: "test:z".into(),
            price: 9.0,
            currency_type: "EUR".into(),
            payment_method: 1,
            transfers: None,
            transfer_duration: None,
        });
        epoch.fare_rules.push(FareRule {
            fare_id: "test:z".into(),
            route_id: Some("test:r1".into()),
            origin_id: Some("test:zoneA".into()),
            destination_id: Some("test:zoneB".into()),
            contains_id: None,
        });
        let j = sample_journey();
        let est = estimate_journey_fare(&epoch, &j).expect("zone fare");
        assert!((est.amount - 9.0).abs() < 1e-9);
        assert_eq!(est.fare_ids, vec!["test:z".to_string()]);
    }

    #[test]
    fn contains_id_requires_zone_on_journey() {
        let mut epoch = base_epoch();
        epoch.stops = vec![
            stop_zoned("test:A", "A", "test:zoneA"),
            stop_zoned("test:B", "B", "test:zoneB"),
        ];
        epoch.fares.push(FareAttribute {
            fare_id: "test:c".into(),
            price: 3.0,
            currency_type: "EUR".into(),
            payment_method: 1,
            transfers: None,
            transfer_duration: None,
        });
        epoch.fare_rules.push(FareRule {
            fare_id: "test:c".into(),
            route_id: Some("test:r1".into()),
            origin_id: None,
            destination_id: None,
            contains_id: Some("test:zoneC".into()),
        });
        let j = sample_journey();
        assert!(estimate_journey_fare(&epoch, &j).is_none());

        epoch.fare_rules[0].contains_id = Some("test:zoneA".into());
        let est = estimate_journey_fare(&epoch, &j).expect("contains A");
        assert!((est.amount - 3.0).abs() < 1e-9);
    }

    #[test]
    fn prefer_more_specific_zone_over_network_wide() {
        let mut epoch = base_epoch();
        epoch.stops = vec![
            stop_zoned("test:A", "A", "test:zoneA"),
            stop_zoned("test:B", "B", "test:zoneB"),
        ];
        epoch.fares.extend([
            FareAttribute {
                fare_id: "test:net".into(),
                price: 1.0,
                currency_type: "EUR".into(),
                payment_method: 1,
                transfers: Some(0),
                transfer_duration: None,
            },
            FareAttribute {
                fare_id: "test:od".into(),
                price: 5.0,
                currency_type: "EUR".into(),
                payment_method: 1,
                transfers: Some(0),
                transfer_duration: None,
            },
        ]);
        epoch.fare_rules.extend([
            FareRule {
                fare_id: "test:net".into(),
                route_id: None,
                origin_id: None,
                destination_id: None,
                contains_id: None,
            },
            FareRule {
                fare_id: "test:od".into(),
                route_id: Some("test:r1".into()),
                origin_id: Some("test:zoneA".into()),
                destination_id: Some("test:zoneB".into()),
                contains_id: None,
            },
        ]);
        let j = sample_journey();
        let est = estimate_journey_fare(&epoch, &j).expect("specific");
        // Only the more specific OD product (spec 8) — not sum with network-wide (spec 0).
        assert!((est.amount - 5.0).abs() < 1e-9);
        assert_eq!(est.fare_ids, vec!["test:od".to_string()]);
    }

    #[test]
    fn network_wide_and_route_sum_unique_same_specificity() {
        let mut epoch = base_epoch();
        // Two route-level products (same specificity class via different routes would
        // not both match; same route + network: different specificity → only route wins.
        // Same specificity: two rules with only route_id None for two fares both match
        // network-wide only if we only have network rules.
        epoch.fares.extend([
            FareAttribute {
                fare_id: "test:a".into(),
                price: 1.0,
                currency_type: "EUR".into(),
                payment_method: 1,
                transfers: Some(0),
                transfer_duration: None,
            },
            FareAttribute {
                fare_id: "test:b".into(),
                price: 3.0,
                currency_type: "EUR".into(),
                payment_method: 1,
                transfers: Some(0),
                transfer_duration: None,
            },
        ]);
        epoch.fare_rules.extend([
            FareRule {
                fare_id: "test:a".into(),
                route_id: None,
                origin_id: None,
                destination_id: None,
                contains_id: None,
            },
            FareRule {
                fare_id: "test:b".into(),
                route_id: None,
                origin_id: None,
                destination_id: None,
                contains_id: None,
            },
        ]);
        let j = sample_journey();
        let est = estimate_journey_fare(&epoch, &j).expect("fare");
        assert!((est.amount - 4.0).abs() < 1e-9);
        assert_eq!(est.fare_ids.len(), 2);
    }

    #[test]
    fn route_beats_network_specificity() {
        let mut epoch = base_epoch();
        epoch.fares.extend([
            FareAttribute {
                fare_id: "test:net".into(),
                price: 10.0,
                currency_type: "EUR".into(),
                payment_method: 1,
                transfers: Some(0),
                transfer_duration: None,
            },
            FareAttribute {
                fare_id: "test:route".into(),
                price: 2.0,
                currency_type: "EUR".into(),
                payment_method: 1,
                transfers: Some(0),
                transfer_duration: None,
            },
        ]);
        epoch.fare_rules.extend([
            FareRule {
                fare_id: "test:net".into(),
                route_id: None,
                origin_id: None,
                destination_id: None,
                contains_id: None,
            },
            FareRule {
                fare_id: "test:route".into(),
                route_id: Some("test:r1".into()),
                origin_id: None,
                destination_id: None,
                contains_id: None,
            },
        ]);
        let j = sample_journey();
        let est = estimate_journey_fare(&epoch, &j).expect("route preferred");
        assert!((est.amount - 2.0).abs() < 1e-9);
        assert_eq!(est.fare_ids, vec!["test:route".to_string()]);
    }

    #[test]
    fn walk_only_no_fare() {
        let mut epoch = base_epoch();
        epoch.fares.push(FareAttribute {
            fare_id: "test:flat".into(),
            price: 2.5,
            currency_type: "EUR".into(),
            payment_method: 1,
            transfers: Some(0),
            transfer_duration: None,
        });
        epoch.fare_rules.push(FareRule {
            fare_id: "test:flat".into(),
            route_id: None,
            origin_id: None,
            destination_id: None,
            contains_id: None,
        });
        let t0 = Utc.with_ymd_and_hms(2026, 6, 1, 8, 0, 0).unwrap();
        let j = Journey {
            id: new_journey_id(),
            departure: t0,
            arrival: t0,
            duration_s: 0,
            transfers: 0,
            walk_distance_m: 100.0,
            realtime_status: "SCHEDULED".into(),
            legs: vec![Leg::Walk(WalkLegData {
                    mode: "WALK".into(),
                from_name: "A".into(),
                to_name: "B".into(),
                from_stop_id: Some("test:A".into()),
                to_stop_id: Some("test:B".into()),
                distance_m: 100.0,
                duration_s: 120,
                from_lat: None,
                from_lon: None,
                to_lat: None,
                to_lon: None,
                geometry: vec![],
            })],
            alert_headers: vec![],
            fare_amount: None,
            fare_currency: None,
            fare_note: None,
        };
        assert!(estimate_journey_fare(&epoch, &j).is_none());
    }

    #[test]
    fn wrong_route_no_match() {
        let mut epoch = base_epoch();
        epoch.fares.push(FareAttribute {
            fare_id: "test:other".into(),
            price: 5.0,
            currency_type: "EUR".into(),
            payment_method: 1,
            transfers: Some(0),
            transfer_duration: None,
        });
        epoch.fare_rules.push(FareRule {
            fare_id: "test:other".into(),
            route_id: Some("test:r99".into()),
            origin_id: None,
            destination_id: None,
            contains_id: None,
        });
        let j = sample_journey();
        assert!(estimate_journey_fare(&epoch, &j).is_none());
    }
}
