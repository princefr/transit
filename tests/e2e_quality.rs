//! Quality / edge-case e2e: alerts (lang + HTML + dedupe), display labels,
//! multi-stop access for same-complex transfers, tiny GTFS itinerary.

use chrono::{TimeZone, Utc};
use std::io::{Cursor, Write};
use std::sync::Arc;
use transit::gtfs::pack::build_epoch;
use transit::gtfs::parse::load_gtfs_bytes;
use transit::routing::{plan_journeys, ItineraryQuery};
use transit::rt::alert_text::{
    alert_active_now, alert_dedupe_key, clean_alert_text, pick_translation,
};
use transit::rt::decode::apply_service_alerts;
use transit::rt::overlay::{AlertRt, FeedRtState};
use transit::api::graphql::{compute_display_label, humanize_rt_ref, map_alert};
use zip::write::SimpleFileOptions;
use zip::ZipWriter;

fn tiny_metro_zip() -> Vec<u8> {
    // Two stations, each with two platforms a few metres apart (same complex).
    // Trip only boards platform A1 and alights B1 — origin station A must expand
    // to A1, destination station B must expand / nearby to B1.
    let buf = Cursor::new(Vec::new());
    let mut zip = ZipWriter::new(buf);
    let opts = SimpleFileOptions::default();

    zip.start_file("agency.txt", opts).unwrap();
    zip.write_all(b"agency_id,agency_name,agency_timezone\na1,IDFM Test,Europe/Paris\n")
        .unwrap();

    zip.start_file("stops.txt", opts).unwrap();
    zip.write_all(
        b"stop_id,stop_name,stop_lat,stop_lon,location_type,parent_station\n\
          AUL,Aulnay-sous-Bois,48.9320,2.4950,1,\n\
          AUL_B,Aulnay RER B,48.9321,2.4951,0,AUL\n\
          CLH,Chatelet-Les Halles,48.8616,2.3470,1,\n\
          CLH_B,Chatelet RER B,48.8617,2.3471,0,CLH\n\
          CLH_A,Chatelet RER A,48.8615,2.3469,0,CLH\n",
    )
    .unwrap();

    zip.start_file("routes.txt", opts).unwrap();
    zip.write_all(
        b"route_id,route_short_name,route_long_name,route_type,agency_id\n\
          RB,B,RER B,2,a1\n",
    )
    .unwrap();

    zip.start_file("calendar.txt", opts).unwrap();
    zip.write_all(
        b"service_id,monday,tuesday,wednesday,thursday,friday,saturday,sunday,start_date,end_date\n\
          S1,1,1,1,1,1,1,1,20260101,20261231\n",
    )
    .unwrap();

    zip.start_file("trips.txt", opts).unwrap();
    zip.write_all(b"route_id,service_id,trip_id,trip_headsign\nRB,S1,trip_b,Robinson\n")
        .unwrap();

    zip.start_file("stop_times.txt", opts).unwrap();
    zip.write_all(
        b"trip_id,arrival_time,departure_time,stop_id,stop_sequence\n\
          trip_b,08:00:00,08:00:00,AUL_B,1\n\
          trip_b,08:25:00,08:25:00,CLH_B,2\n",
    )
    .unwrap();

    zip.finish().unwrap().into_inner()
}

fn query(from: &str, to: &str) -> ItineraryQuery {
    // 06:00 UTC ≈ 08:00 Europe/Paris (CEST) — leave headroom before 08:00 trip
    let departure_at = Utc.with_ymd_and_hms(2026, 7, 27, 5, 0, 0).unwrap();
    ItineraryQuery {
        from_stop_id: Some(from.into()),
        to_stop_id: Some(to.into()),
        from_lat: None,
        from_lon: None,
        to_lat: None,
        to_lon: None,
        departure_at,
        arrive_by: false,
        max_transfers: 4,
        max_results: 3,
        modes: None,
        max_walk_meters: 800,
        walk_speed_m_s: 1.2,
        raptor_max_rounds: 6,
        default_transfer_s: 120,
        timezone: "Europe/Paris".into(),
        excluded_trip_ids: Default::default(),
        rt_adjust: Default::default(),
        wheelchair: false,
        osrm_url: None,
            bike_from: false,
            bike_to: false,
            bike_speed_m_s: 4.2,
            max_bike_meters: 5000,
    }
}

#[test]
fn e2e_aulnay_chatelet_style_itinerary_via_station_nodes() {
    let zip = tiny_metro_zip();
    let bundle = load_gtfs_bytes("idfm", &zip).expect("parse");
    let epoch = Arc::new(build_epoch(vec![Arc::new(bundle)], vec![]));

    // Plan from parent stations (what the UI often picks), not platform ids.
    let res = plan_journeys(&epoch, &query("idfm:AUL", "idfm:CLH"));
    assert!(
        !res.journeys.is_empty(),
        "expected at least one journey Aulnay→Châtelet-style via RER B platforms"
    );
    let j = &res.journeys[0];
    assert!(j.duration_s > 0);
    assert!(
        j.legs
            .iter()
            .any(|l| matches!(l, transit::routing::journey::Leg::Transit(_))),
        "expected a transit leg"
    );
}

#[test]
fn e2e_alert_prefers_french_and_strips_html() {
    use prost::Message;
    use transit::rt::decode::pb::feed_header::Incrementality;
    use transit::rt::decode::pb::translated_string::Translation;
    use transit::rt::decode::pb::{Alert, FeedEntity, FeedHeader, FeedMessage, TranslatedString};

    let alert = Alert {
        active_period: vec![],
        informed_entity: vec![],
        cause: None,
        effect: None,
        url: None,
        header_text: Some(TranslatedString {
            translation: vec![
                Translation {
                    text: "Den Zug am Gleisende in Marseille besteigen".into(),
                    language: Some("de".into()),
                },
                Translation {
                    text: "Empruntez le train en tête de quai à Marseille".into(),
                    language: Some("fr".into()),
                },
            ],
        }),
        description_text: Some(TranslatedString {
            translation: vec![
                Translation {
                    text: "<p>Ihr Zug 879502 wird mit dem Zugverband gefahren</p>".into(),
                    language: Some("de".into()),
                },
                Translation {
                    text: "<p>Votre train <b>879502</b> circule en unité multiple.</p>".into(),
                    language: Some("fr".into()),
                },
            ],
        }),
        tts_header_text: None,
        tts_description_text: None,
        severity_level: None,
        cause_detail: None,
        effect_detail: None,
        image: None,
        image_alternative_text: None,
        communication_period: vec![],
        impact_period: vec![],
    };

    let msg = FeedMessage {
        header: FeedHeader {
            gtfs_realtime_version: "2.0".into(),
            incrementality: Some(Incrementality::FullDataset as i32),
            timestamp: Some(1_700_000_000),
            feed_version: None,
        },
        entity: vec![
            FeedEntity {
                id: "QOM:1".into(),
                is_deleted: None,
                trip_update: None,
                vehicle: None,
                alert: Some(alert),
                shape: None,
                stop: None,
                trip_modifications: None,
            },
            // Second entity: same copy (rebuild) — SNCF QOM clones
            FeedEntity {
                id: "QOM:2".into(),
                is_deleted: None,
                trip_update: None,
                vehicle: None,
                alert: Some(Alert {
                    active_period: vec![],
                    informed_entity: vec![],
                    cause: None,
                    effect: None,
                    url: None,
                    header_text: Some(TranslatedString {
                        translation: vec![
                            Translation {
                                text: "Den Zug am Gleisende in Marseille besteigen".into(),
                                language: Some("de".into()),
                            },
                            Translation {
                                text: "Empruntez le train en tête de quai à Marseille".into(),
                                language: Some("fr".into()),
                            },
                        ],
                    }),
                    description_text: Some(TranslatedString {
                        translation: vec![
                            Translation {
                                text: "<p>Ihr Zug 879502 wird mit dem Zugverband gefahren</p>"
                                    .into(),
                                language: Some("de".into()),
                            },
                            Translation {
                                text: "<p>Votre train <b>879502</b> circule en unité multiple.</p>"
                                    .into(),
                                language: Some("fr".into()),
                            },
                        ],
                    }),
                    tts_header_text: None,
                    tts_description_text: None,
                    severity_level: None,
                    cause_detail: None,
                    effect_detail: None,
                    image: None,
                    image_alternative_text: None,
                    communication_period: vec![],
                    impact_period: vec![],
                }),
                shape: None,
                stop: None,
                trip_modifications: None,
            },
        ],
    };
    let mut buf = Vec::new();
    msg.encode(&mut buf).unwrap();

    let mut state = FeedRtState::new("sncf");
    apply_service_alerts("sncf", &buf, &mut state);
    assert_eq!(state.alerts.len(), 2);

    let a = &state.alerts[0];
    assert_eq!(
        a.header.as_deref(),
        Some("Empruntez le train en tête de quai à Marseille")
    );
    let desc = a.description.as_deref().unwrap_or("");
    assert!(!desc.contains('<'), "HTML must be stripped: {desc}");
    assert!(desc.contains("879502"));

    let m0 = map_alert(a);
    let m1 = map_alert(&state.alerts[1]);
    let k0 = alert_dedupe_key(m0.header.as_deref(), m0.description.as_deref());
    let k1 = alert_dedupe_key(m1.header.as_deref(), m1.description.as_deref());
    assert_eq!(k0, k1, "duplicate broadcasts share dedupe key");
}

#[test]
fn e2e_display_label_skips_service_dates() {
    assert_eq!(
        compute_display_label(None, None, Some("B"), Some("Robinson"), None),
        "B → Robinson"
    );
    // SNCF trip ids often end with YYYYMMDD — must not become the badge
    let label = compute_display_label(
        None,
        None,
        None,
        None,
        Some("sncf:OCESN7856F1187_F:...:20260830"),
    );
    assert!(!label.chars().all(|c| c.is_ascii_digit()));
    assert_ne!(label, "20260830");
    assert!(humanize_rt_ref("sncf:foo:20260727").is_some());
    let h = humanize_rt_ref("sncf:OCESN7856:20260830").unwrap();
    assert_ne!(h, "20260830");
}

#[test]
fn e2e_alert_text_unit_helpers() {
    assert_eq!(
        pick_translation(&[
            ("de".into(), "Hallo".into()),
            ("fr".into(), "Bonjour".into()),
        ])
        .as_deref(),
        Some("Bonjour")
    );
    assert_eq!(
        clean_alert_text("<p>Hi<br/>there</p>"),
        "Hi there"
    );
    assert!(alert_active_now(&[(Some(0), Some(i64::MAX))], 100));
}

#[test]
fn e2e_map_alert_strips_html_without_translations() {
    let a = AlertRt {
        id: "x".into(),
        header: Some("<b>INFO</b>".into()),
        description: Some("<p>Works on <span>line</span></p>".into()),
        severity: "INFO".into(),
        informed_stop_ids: vec![],
        informed_route_ids: vec![],
        informed_trip_ids: vec![],
        cause: None,
        effect: None,
        url: None,
        active_start: None,
        active_end: None,
        active_periods: vec![],
        header_translations: vec![],
    };
    let m = map_alert(&a);
    assert_eq!(m.header.as_deref(), Some("INFO"));
    assert_eq!(m.description.as_deref(), Some("Works on line"));
}
