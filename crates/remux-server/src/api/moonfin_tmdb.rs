//! Moonfin next-episode badge (`GET /Moonfin/Tmdb/NextEpisode?tmdbId=`).
//!
//! Moonbase answers this from TMDB's `next_episode_to_air`; Remux already
//! stores every episode's air date, so it is answered from the local
//! database with no network call and no TMDB key.
//!
//! * Reply is always `200`: `{"success":true,"airDate":"YYYY-MM-DD",
//!   "seasonNumber":n,"episodeNumber":n,"name":"..."}`, or
//!   `{"success":false}` when nothing is scheduled (unknown or hidden
//!   series, ended series, no dated episode ahead). The client swallows
//!   anything else as "no data".
//! * A missing or non-numeric `tmdbId` is `400 {"Error":...}`.
//! * `airDate` is date-only: the client parses it as local midnight, so a
//!   timestamp would show the badge a day early west of UTC.
//! * An episode airing today counts (the client only hides dates strictly
//!   before its local today). "Today" is the server's UTC date, which is how
//!   air dates are stored (midnight UTC).
//! * Specials (season 0) are ignored.
//! * Only series the signed-in user may see (parental rating, tags, filter
//!   rules) are considered.

use axum::{
    Json,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use chrono::{NaiveDate, Utc};
use remux_macros::get;
use remux_sdks::remux::AnyProviderIds;
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

use crate::{
    AppState,
    api::moonfin_settings::{error_response, internal_error},
    db::{self, auth::AuthSession},
};

#[derive(Deserialize)]
pub struct NextEpisodeQuery {
    #[serde(rename = "tmdbId", alias = "tmdbid", alias = "TmdbId")]
    tmdb_id: Option<String>,
}

/// The next scheduled regular episode of a series.
#[derive(Debug, PartialEq, Eq)]
struct NextEpisode {
    air_date: NaiveDate,
    season: i64,
    episode: i64,
    name: String,
}

/// Earliest episode on or after `today` across `series_ids`, season 0
/// (specials) excluded, ties broken by season then episode number.
async fn next_episode(
    db: &sqlx::SqlitePool,
    series_ids: &[Uuid],
    today: NaiveDate,
) -> sqlx::Result<Option<NextEpisode>> {
    if series_ids.is_empty() {
        return Ok(None);
    }
    let mut qb = sqlx::QueryBuilder::new(
        "SELECT released_at, parent_idx, idx, title FROM media \
         WHERE kind = 'episode' AND parent_idx > 0 AND idx IS NOT NULL \
         AND released_at >= ",
    );
    qb.push_bind(
        today
            .and_hms_opt(0, 0, 0)
            .expect("midnight is valid"),
    );
    qb.push(" AND grandparent_id IN (");
    let mut sep = qb.separated(", ");
    for id in series_ids {
        sep.push_bind(*id);
    }
    qb.push(") ORDER BY released_at, parent_idx, idx LIMIT 1");
    let Some(row) = qb
        .build()
        .fetch_optional(db)
        .await?
    else {
        return Ok(None);
    };
    let released: chrono::NaiveDateTime = row.try_get("released_at")?;
    Ok(Some(NextEpisode {
        air_date: released.date(),
        season: row.try_get("parent_idx")?,
        episode: row.try_get("idx")?,
        name: row.try_get("title")?,
    }))
}

#[get("/moonfin/tmdb/nextepisode")]
pub async fn moonfin_tmdb_next_episode(
    State(state): State<AppState>,
    session: AuthSession,
    Query(q): Query<NextEpisodeQuery>,
) -> Response {
    let Some(tmdb_id) = q
        .tmdb_id
        .as_deref()
        .and_then(|v| {
            v.trim()
                .parse::<i64>()
                .ok()
        })
    else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Missing required parameter: tmdbId",
        );
    };
    let none = || Json(json!({ "success": false })).into_response();

    // Series the user can see, honouring their policy like browsing does.
    let policy = session
        .user
        .policy
        .as_ref()
        .map(|p| &p.0);
    let visible = db::Media::get_by_filter(
        &state
            .ctx
            .db,
        &db::MediaFilter {
            kind: Some(vec![db::MediaKind::Series]),
            any_provider_ids: Some(AnyProviderIds {
                tmdb: vec![tmdb_id],
                ..Default::default()
            }),
            max_parental_rating: policy.and_then(|p| p.max_parental_rating),
            blocked_tags: policy
                .map(|p| {
                    p.blocked_tags
                        .clone()
                })
                .filter(|v| !v.is_empty()),
            allowed_tags: policy
                .map(|p| {
                    p.allowed_tags
                        .clone()
                })
                .filter(|v| !v.is_empty()),
            policy_filter: policy.and_then(|p| {
                p.filter_rules
                    .clone()
            }),
            ..Default::default()
        },
    )
    .await;
    let series_ids: Vec<Uuid> = match visible {
        Ok(result) => result
            .records
            .into_iter()
            .map(|m| m.id)
            .collect(),
        Err(e) => return internal_error(format!("{e:?}")),
    };

    match next_episode(
        &state
            .ctx
            .db,
        &series_ids,
        Utc::now().date_naive(),
    )
    .await
    {
        Ok(Some(next)) => Json(json!({
            "success": true,
            "airDate": next.air_date.format("%Y-%m-%d").to_string(),
            "seasonNumber": next.season,
            "episodeNumber": next.episode,
            "name": next.name,
        }))
        .into_response(),
        Ok(None) => none(),
        Err(e) => internal_error(e),
    }
}

#[cfg(test)]
mod tests {
    use crate::api::moonfin_test_support::Harness;
    use crate::db;
    use chrono::{Days, NaiveDateTime, Utc};
    use http::StatusCode;
    use serde_json::{Value, json};

    fn day(offset: i64) -> NaiveDateTime {
        let today = Utc::now().date_naive();
        let date = if offset >= 0 {
            today + Days::new(offset as u64)
        } else {
            today - Days::new(offset.unsigned_abs())
        };
        date.and_hms_opt(0, 0, 0)
            .unwrap()
    }

    async fn series(h: &Harness, tmdb: i64, age: Option<i32>) -> db::Media {
        let external_ids = db::ExternalIds {
            tmdb: Some(tmdb),
            ..Default::default()
        };
        let mut media = db::Media {
            id: uuid::Uuid::from(&db::MediaIdRaw {
                kind: db::MediaKind::Series,
                external_ids: external_ids.clone(),
                season: None,
                episode: None,
            }),
            title: format!("Show {tmdb}"),
            kind: db::MediaKind::Series,
            external_ids,
            certification_age: age,
            certification: age.map(|a| format!("age-{a}")),
            ..Default::default()
        };
        media
            .save(
                &h.ctx
                    .db,
            )
            .await
            .unwrap();
        media
    }

    async fn episode(
        h: &Harness,
        series: &db::Media,
        season: i64,
        number: i64,
        title: &str,
        air: Option<NaiveDateTime>,
    ) {
        let mut ep = db::Media {
            id: crate::common::stable_media_uuid(
                &db::MediaKind::Episode,
                &format!("{}:{season}:{number}", series.id),
            ),
            title: title.into(),
            kind: db::MediaKind::Episode,
            grandparent_id: Some(series.id),
            parent_id: Some(series.id),
            parent_idx: Some(season),
            idx: Some(number),
            released_at: air,
            ..Default::default()
        };
        ep.save(
            &h.ctx
                .db,
        )
        .await
        .unwrap();
    }

    async fn next(h: &Harness, token: &str, query: &str) -> Value {
        let resp = h
            .get(token, &format!("/Moonfin/Tmdb/NextEpisode{query}"))
            .await;
        resp.assert_status_ok();
        resp.json::<Value>()
    }

    fn date_str(offset: i64) -> String {
        day(offset)
            .format("%Y-%m-%d")
            .to_string()
    }

    #[tokio::test]
    async fn returns_the_soonest_future_episode_with_a_date_only_air_date() {
        let h = Harness::new().await;
        let s = series(&h, 1399, None).await;
        episode(&h, &s, 1, 1, "Pilot", Some(day(-400))).await;
        episode(&h, &s, 1, 2, "Second", Some(day(-390))).await;
        episode(&h, &s, 2, 4, "Later", Some(day(10))).await;
        episode(&h, &s, 2, 3, "Soon", Some(day(3))).await;

        let body = next(&h, &h.token, "?tmdbId=1399").await;
        assert_eq!(
            body,
            json!({
                "success": true,
                "airDate": date_str(3),
                "seasonNumber": 2,
                "episodeNumber": 3,
                "name": "Soon",
            })
        );
        let air = body["airDate"]
            .as_str()
            .unwrap();
        assert_eq!(air.len(), 10);
        assert!(
            air.chars()
                .enumerate()
                .all(|(i, c)| if i == 4 || i == 7 {
                    c == '-'
                } else {
                    c.is_ascii_digit()
                })
        );
    }

    #[tokio::test]
    async fn an_episode_airing_today_counts_but_yesterdays_does_not() {
        let h = Harness::new().await;
        let today = series(&h, 2001, None).await;
        episode(&h, &today, 1, 1, "Tonight", Some(day(0))).await;
        assert_eq!(
            next(&h, &h.token, "?tmdbId=2001").await["airDate"],
            date_str(0)
        );

        let yesterday = series(&h, 2002, None).await;
        episode(&h, &yesterday, 1, 1, "Gone", Some(day(-1))).await;
        assert_eq!(
            next(&h, &h.token, "?tmdbId=2002").await,
            json!({ "success": false })
        );
    }

    #[tokio::test]
    async fn ended_unknown_and_episodeless_series_are_success_false() {
        let h = Harness::new().await;
        let ended = series(&h, 3001, None).await;
        episode(&h, &ended, 1, 1, "Old", Some(day(-30))).await;
        episode(&h, &ended, 1, 2, "Undated", None).await;
        series(&h, 3002, None).await;
        for id in [3001, 3002, 999999] {
            assert_eq!(
                next(&h, &h.token, &format!("?tmdbId={id}")).await,
                json!({ "success": false })
            );
        }
    }

    #[tokio::test]
    async fn specials_are_ignored() {
        let h = Harness::new().await;
        let s = series(&h, 4001, None).await;
        episode(&h, &s, 0, 1, "Special", Some(day(1))).await;
        episode(&h, &s, 3, 1, "Real", Some(day(20))).await;
        let body = next(&h, &h.token, "?tmdbId=4001").await;
        assert_eq!(body["name"], "Real");
        assert_eq!(body["seasonNumber"], 3);

        let only_special = series(&h, 4002, None).await;
        episode(&h, &only_special, 0, 1, "Special", Some(day(1))).await;
        assert_eq!(
            next(&h, &h.token, "?tmdbId=4002").await,
            json!({ "success": false })
        );
    }

    #[tokio::test]
    async fn only_the_requested_series_is_considered() {
        let h = Harness::new().await;
        let a = series(&h, 5001, None).await;
        let b = series(&h, 5002, None).await;
        episode(&h, &a, 1, 1, "A", Some(day(2))).await;
        episode(&h, &b, 1, 1, "B", Some(day(5))).await;
        assert_eq!(next(&h, &h.token, "?tmdbId=5002").await["name"], "B");
    }

    #[tokio::test]
    async fn missing_or_bad_tmdb_id_is_400() {
        let h = Harness::new().await;
        for q in ["", "?tmdbId=", "?tmdbId=abc"] {
            let resp = h
                .get(&h.token, &format!("/Moonfin/Tmdb/NextEpisode{q}"))
                .await;
            resp.assert_status(StatusCode::BAD_REQUEST);
            assert_eq!(
                resp.json::<Value>(),
                json!({ "Error": "Missing required parameter: tmdbId" })
            );
        }
    }

    #[tokio::test]
    async fn requires_authentication_and_accepts_any_param_casing() {
        let h = Harness::new().await;
        h.server
            .get("/Moonfin/Tmdb/NextEpisode?tmdbId=1")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        let s = series(&h, 6001, None).await;
        episode(&h, &s, 1, 1, "E", Some(day(2))).await;
        for q in ["?tmdbid=6001", "?TmdbId=6001"] {
            assert_eq!(next(&h, &h.token, q).await["success"], true);
        }
    }

    #[tokio::test]
    async fn series_hidden_by_the_users_policy_is_success_false() {
        let h = Harness::new().await;
        let kid = h
            .second_user()
            .await;
        let users = h
            .get(&h.token, "/Users")
            .await
            .json::<Value>();
        let second = users
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["Name"] == "second")
            .unwrap();
        let id = second["Id"]
            .as_str()
            .unwrap();
        let mut policy = second["Policy"].clone();
        policy["MaxParentalRating"] = json!(13);
        h.post(&h.token, &format!("/Users/{id}/Policy"), &policy)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        let mild = series(&h, 7001, Some(10)).await;
        let adult = series(&h, 7002, Some(17)).await;
        episode(&h, &mild, 1, 1, "Mild", Some(day(2))).await;
        episode(&h, &adult, 1, 1, "Adult", Some(day(2))).await;

        assert_eq!(next(&h, &kid, "?tmdbId=7001").await["name"], "Mild");
        assert_eq!(
            next(&h, &kid, "?tmdbId=7002").await,
            json!({ "success": false })
        );
        // The unrestricted admin still sees it.
        assert_eq!(next(&h, &h.token, "?tmdbId=7002").await["name"], "Adult");
    }
}
