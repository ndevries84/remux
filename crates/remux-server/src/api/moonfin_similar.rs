//! Moonfin "Moonfin Recommends" (`GET /Moonfin/Items/{id}/Similar`).
//!
//! A thin wrapper over the native genre-overlap scoring behind
//! `/Items/{id}/Similar` ([`similar_items_for`]), shaped like Moonbase's
//! endpoint: `{ "Items": [...], "TotalRecordCount": n }`.
//!
//! * `limit` defaults to 20 and is capped at 200 (the client asks for up to
//!   100; the native endpoint caps at 50).
//! * `excludeItemIds` (repeated and/or comma separated ids) are dropped
//!   before the limit is applied.
//! * The signed-in user's policy (parental rating, blocked/allowed tags,
//!   filter rules) hides titles the way browsing does. The native endpoint
//!   does not apply it; only this wrapper does.
//! * `userId` is accepted but the signed-in user is always used, so another
//!   user's watch state is never read.
//! * An unknown item, or one the user's policy hides, is a 404; an item with no genres (or an episode) is an
//!   empty list.
//! * Items are the native `BaseItemDto`s, a superset of Moonbase's cards.
//!   The client only reads `Id`, `UserData.Played`, `OfficialRating` and
//!   image tags.

use axum::{
    Json,
    extract::{Path, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use remux_macros::get;
use serde_json::json;
use uuid::Uuid;

use crate::{
    AppState,
    api::{
        items::{SimilarItemsRequest, apply_user_policy, similar_items_for},
        moonfin_settings::{error_response, internal_error},
    },
    db::{self, auth::AuthSession},
};

const DEFAULT_LIMIT: u32 = 20;
const MAX_LIMIT: u32 = 200;

/// Query parameters, matched case-insensitively (the client sends
/// `excludeItemIds`, Jellyfin tooling `ExcludeItemIds`).
struct SimilarQuery {
    limit: u32,
    exclude: Vec<Uuid>,
}

impl SimilarQuery {
    fn parse(raw: Option<&str>) -> Self {
        let pairs: Vec<(String, String)> = raw
            .and_then(|q| serde_urlencoded::from_str(q).ok())
            .unwrap_or_default();
        let mut limit = DEFAULT_LIMIT;
        let mut exclude = Vec::new();
        for (key, value) in pairs {
            if key.eq_ignore_ascii_case("limit") {
                if let Ok(n) = value.parse::<u32>() {
                    limit = n.min(MAX_LIMIT);
                }
            } else if key.eq_ignore_ascii_case("excludeItemIds") {
                exclude.extend(
                    value
                        .split(',')
                        .filter_map(|id| Uuid::parse_str(id.trim()).ok()),
                );
            }
        }
        Self { limit, exclude }
    }
}

#[get("/moonfin/items/{id}/similar")]
pub async fn moonfin_similar(
    State(state): State<AppState>,
    session: AuthSession,
    Path(id): Path<Uuid>,
    RawQuery(raw): RawQuery,
) -> Response {
    let q = SimilarQuery::parse(raw.as_deref());
    // The source item must be visible to the user under the same policy that
    // filters the candidates; otherwise hidden titles would leak via 404 vs 200.
    let mut visible = db::MediaFilter {
        id: Some(vec![id]),
        user_id: Some(
            session
                .user
                .id,
        ),
        ..Default::default()
    };
    apply_user_policy(&mut visible, &session.user);
    let media = match db::Media::get_by_filter(
        &state
            .ctx
            .db,
        &visible,
    )
    .await
    {
        Ok(result) => match result
            .records
            .into_iter()
            .next()
        {
            Some(media) => media,
            None => return error_response(StatusCode::NOT_FOUND, "Item not found"),
        },
        Err(e) => return internal_error(e),
    };
    let result = similar_items_for(
        &state,
        SimilarItemsRequest {
            id: media.id,
            user_id: session
                .user
                .id,
            limit: q.limit,
            offset: 0,
            exclude: &q.exclude,
            user: &session.user,
            fields: None,
        },
    )
    .await;
    match result {
        Ok(result) => {
            let count = result
                .items
                .len();
            Json(json!({ "Items": result.items, "TotalRecordCount": count }))
                .into_response()
        }
        Err(e) => internal_error(format!("{e:?}")),
    }
}

#[cfg(test)]
mod tests {
    use crate::api::moonfin_test_support::{Harness, seed_movie_full};
    use http::StatusCode;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicI64, Ordering};

    /// Genre names are suffixed so they can never collide with the genres of
    /// real catalog items the server syncs in the background while a test
    /// runs.
    async fn movie(
        h: &Harness,
        title: &str,
        genres: &[&str],
        age: Option<i32>,
    ) -> String {
        static NEXT: AtomicI64 = AtomicI64::new(5000);
        let genres: Vec<String> = genres
            .iter()
            .map(|g| format!("{g}-moonfin-test"))
            .collect();
        let genres: Vec<&str> = genres
            .iter()
            .map(String::as_str)
            .collect();
        seed_movie_full(
            &h.ctx,
            title,
            NEXT.fetch_add(1, Ordering::Relaxed),
            &genres,
            age,
        )
        .await
    }

    /// Item ids in a response, asserting the client-visible envelope.
    fn ids(body: &Value) -> Vec<String> {
        assert_eq!(
            body["TotalRecordCount"],
            body["Items"]
                .as_array()
                .unwrap()
                .len()
        );
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                i["Id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    async fn similar(h: &Harness, token: &str, id: &str, query: &str) -> Value {
        let resp = h
            .get(token, &format!("/Moonfin/Items/{id}/Similar{query}"))
            .await;
        resp.assert_status_ok();
        resp.json::<Value>()
    }

    async fn restricted_user(h: &Harness, max_rating: i32) -> String {
        let token = h
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
        policy["MaxParentalRating"] = json!(max_rating);
        h.post(&h.token, &format!("/Users/{id}/Policy"), &policy)
            .await
            .assert_status(StatusCode::NO_CONTENT);
        token
    }

    #[tokio::test]
    async fn requires_authentication() {
        let h = Harness::new().await;
        let id = movie(&h, "Seed", &["Action"], None).await;
        h.server
            .get(&format!("/Moonfin/Items/{id}/Similar"))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn returns_items_sharing_genres_but_never_the_seed() {
        let h = Harness::new().await;
        let seed = movie(&h, "Seed", &["Action", "Comedy"], None).await;
        let both = movie(&h, "Both", &["Action", "Comedy"], None).await;
        let one = movie(&h, "One", &["Action"], None).await;
        let _other = movie(&h, "Other", &["Drama"], None).await;

        let body = similar(&h, &h.token, &seed, "?limit=10&userId=ignored").await;
        // Best overlap first.
        assert_eq!(ids(&body), vec![both, one]);
    }

    #[tokio::test]
    async fn limit_above_the_native_cap_of_50_is_honoured() {
        let h = Harness::new().await;
        let seed = movie(&h, "Seed", &["Action"], None).await;
        for i in 0..60 {
            movie(&h, &format!("M{i}"), &["Action"], None).await;
        }
        assert_eq!(
            ids(&similar(&h, &h.token, &seed, "?limit=100").await).len(),
            60
        );
        assert_eq!(
            ids(&similar(&h, &h.token, &seed, "?limit=55").await).len(),
            55
        );
        // Default is 20.
        assert_eq!(ids(&similar(&h, &h.token, &seed, "").await).len(), 20);
        // The native endpoint keeps its own cap.
        let native = h
            .get(&h.token, &format!("/Items/{seed}/Similar?limit=100"))
            .await
            .json::<Value>();
        assert_eq!(
            native["Items"]
                .as_array()
                .unwrap()
                .len(),
            50
        );
    }

    #[tokio::test]
    async fn excluded_ids_are_omitted_and_the_page_is_still_filled() {
        let h = Harness::new().await;
        let seed = movie(&h, "Seed", &["Action"], None).await;
        let mut all = Vec::new();
        for i in 0..6 {
            all.push(movie(&h, &format!("M{i}"), &["Action"], None).await);
        }
        let repeated = format!(
            "?limit=4&excludeItemIds={}&excludeItemIds={}",
            all[0], all[1]
        );
        let got = ids(&similar(&h, &h.token, &seed, &repeated).await);
        assert_eq!(got.len(), 4);
        assert!(!got.contains(&all[0]) && !got.contains(&all[1]));

        let commas = format!("?limit=10&ExcludeItemIds={},{}", all[2], all[3]);
        let got = ids(&similar(&h, &h.token, &seed, &commas).await);
        assert_eq!(got.len(), 4);
        assert!(!got.contains(&all[2]) && !got.contains(&all[3]));
    }

    #[tokio::test]
    async fn items_carry_the_signed_in_users_watch_state() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let seed = movie(&h, "Seed", &["Action"], None).await;
        let watched = movie(&h, "Watched", &["Action"], None).await;
        h.post_empty(&h.token, &format!("/UserPlayedItems/{watched}"))
            .await
            .assert_status_ok();

        let played = |body: &Value| body["Items"][0]["UserData"]["Played"].as_bool();
        assert_eq!(played(&similar(&h, &h.token, &seed, "").await), Some(true));
        // Another user's state is never leaked, even via userId.
        let admin_id = h
            .get(&h.token, "/Users/Me")
            .await
            .json::<Value>()["Id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            played(&similar(&h, &other, &seed, &format!("?userId={admin_id}")).await),
            Some(false)
        );
    }

    #[tokio::test]
    async fn unknown_item_is_404() {
        let h = Harness::new().await;
        h.get(
            &h.token,
            "/Moonfin/Items/00000000000000000000000000000abc/Similar",
        )
        .await
        .assert_status(StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn item_without_genres_gets_an_empty_list() {
        let h = Harness::new().await;
        let bare = movie(&h, "Bare", &[], None).await;
        movie(&h, "Other", &["Action"], None).await;
        assert_eq!(
            similar(&h, &h.token, &bare, "").await,
            json!({ "Items": [], "TotalRecordCount": 0 })
        );
    }

    #[tokio::test]
    async fn over_limit_titles_are_hidden_by_the_users_parental_policy() {
        let h = Harness::new().await;
        let kid = restricted_user(&h, 13).await;
        let seed = movie(&h, "Seed", &["Action"], Some(10)).await;
        let mild = movie(&h, "Mild", &["Action"], Some(10)).await;
        let adult = movie(&h, "Adult", &["Action"], Some(17)).await;

        let admin = ids(&similar(&h, &h.token, &seed, "").await);
        assert!(admin.contains(&mild) && admin.contains(&adult));
        let restricted = ids(&similar(&h, &kid, &seed, "").await);
        assert_eq!(restricted, vec![mild]);
    }

    #[tokio::test]
    async fn source_hidden_by_parental_rating_is_404_but_visible_source_is_unchanged() {
        let h = Harness::new().await;
        let kid = restricted_user(&h, 13).await;
        let adult_seed = movie(&h, "AdultSeed", &["Action"], Some(17)).await;
        let mild_seed = movie(&h, "MildSeed", &["Action"], Some(10)).await;
        let mild = movie(&h, "Mild", &["Action"], Some(10)).await;

        h.get(&kid, &format!("/Moonfin/Items/{adult_seed}/Similar"))
            .await
            .assert_status(StatusCode::NOT_FOUND);
        // Admin (no restriction) still sees the adult source.
        similar(&h, &h.token, &adult_seed, "").await;
        // A visible source behaves as before.
        let got = ids(&similar(&h, &kid, &mild_seed, "").await);
        assert_eq!(got, vec![mild]);
    }

    #[tokio::test]
    async fn policy_hidden_titles_do_not_shorten_the_page() {
        let h = Harness::new().await;
        let kid = restricted_user(&h, 13).await;
        let seed = movie(&h, "Seed", &["Action", "Drama"], Some(10)).await;
        // Higher-scoring adult titles would fill the page without over-fetch.
        for i in 0..5 {
            movie(&h, &format!("Adult{i}"), &["Action", "Drama"], Some(17)).await;
        }
        for i in 0..3 {
            movie(&h, &format!("Mild{i}"), &["Action"], Some(10)).await;
        }
        let got = ids(&similar(&h, &kid, &seed, "?limit=3").await);
        assert_eq!(got.len(), 3);
    }

    #[tokio::test]
    async fn native_similar_endpoint_still_works_with_bypass_param() {
        let h = Harness::new().await;
        let seed = movie(&h, "Seed", &["Action"], None).await;
        let other = movie(&h, "Other", &["Action"], None).await;
        let body = h
            .get(
                &h.token,
                &format!("/Items/{seed}/Similar?Limit=5&bypass=moonfin"),
            )
            .await
            .json::<Value>();
        assert_eq!(body["Items"][0]["Id"], other);
    }
}
