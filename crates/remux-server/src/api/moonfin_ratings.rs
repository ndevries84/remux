//! Moonfin `UserRatings/Mine` (`GET /Moonfin/UserRatings/Mine`).
//!
//! The client's "My Rating" sort fetches every item the signed-in user has
//! rated, sorts by score itself and then loads the items in chunks through
//! the normal `/Items?ids=` endpoint. This is a thin read over the personal
//! ratings written by `POST /UserItems/{id}/Rating`
//! (`user_media_state.rating`, 0-10).
//!
//! `ItemId` is the dashless (`N`) form that `BaseItemDto.Id` serialises as
//! (and Moonbase emits), since the client feeds it straight back into
//! `/Items?ids=`.

use axum::{
    Json,
    extract::State,
    response::{IntoResponse, Response},
};
use remux_macros::get;
use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;

use crate::{AppState, api::moonfin_settings::internal_error, db::auth::AuthSession};

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct RatingEntry {
    item_id: String,
    rating: f64,
    /// Derived from the score like Jellyfin's `UserData.Likes`.
    likes: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct MyRatings {
    items: Vec<RatingEntry>,
}

#[get("/moonfin/userratings/mine")]
pub async fn moonfin_user_ratings_mine(
    State(state): State<AppState>,
    session: AuthSession,
) -> Response {
    let rows = match sqlx::query(
        "SELECT media_id, rating FROM user_media_state \
         WHERE user_id = ?1 AND rating IS NOT NULL \
         ORDER BY media_id",
    )
    .bind(
        session
            .user
            .id,
    )
    .fetch_all(
        &state
            .ctx
            .db,
    )
    .await
    {
        Ok(rows) => rows,
        Err(e) => return internal_error(e),
    };
    let items = rows
        .iter()
        .map(|row| {
            let rating: f64 = row.get("rating");
            RatingEntry {
                item_id: row
                    .get::<Uuid, _>("media_id")
                    .simple()
                    .to_string(),
                rating,
                likes: rating >= crate::db::UserRating::LIKE_THRESHOLD,
            }
        })
        .collect();
    Json(MyRatings { items }).into_response()
}

#[cfg(test)]
mod tests {
    use crate::api::moonfin_test_support::{Harness, seed_movie};
    use http::StatusCode;
    use serde_json::{Value, json};
    use std::sync::atomic::{AtomicI64, Ordering};

    async fn movie(h: &Harness, title: &str) -> String {
        static NEXT: AtomicI64 = AtomicI64::new(1000);
        seed_movie(&h.ctx, title, NEXT.fetch_add(1, Ordering::Relaxed)).await
    }

    async fn rate(h: &Harness, token: &str, id: &str, rating: f64) {
        h.post_empty(token, &format!("/UserItems/{id}/Rating?Rating={rating}"))
            .await
            .assert_status_ok();
    }

    async fn mine(h: &Harness, token: &str) -> Value {
        let resp = h
            .get(token, "/Moonfin/UserRatings/Mine")
            .await;
        resp.assert_status_ok();
        resp.json::<Value>()
    }

    fn rating_of(body: &Value, id: &str) -> Option<f64> {
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["ItemId"] == id)
            .map(|e| {
                e["Rating"]
                    .as_f64()
                    .unwrap()
            })
    }

    #[tokio::test]
    async fn requires_authentication() {
        let h = Harness::new().await;
        h.server
            .get("/Moonfin/UserRatings/Mine")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn no_ratings_is_an_empty_list_not_an_error() {
        let h = Harness::new().await;
        assert_eq!(mine(&h, &h.token).await, json!({ "Items": [] }));
    }

    #[tokio::test]
    async fn rated_items_appear_with_score_and_likes() {
        let h = Harness::new().await;
        let (a, b, c) = (
            movie(&h, "A").await,
            movie(&h, "B").await,
            movie(&h, "C").await,
        );
        movie(&h, "Unrated").await;
        rate(&h, &h.token, &a, 9.0).await;
        rate(&h, &h.token, &b, 5.0).await;
        rate(&h, &h.token, &c, 2.0).await;

        let body = mine(&h, &h.token).await;
        assert_eq!(
            body["Items"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(rating_of(&body, &a), Some(9.0));
        assert_eq!(rating_of(&body, &b), Some(5.0));
        assert_eq!(rating_of(&body, &c), Some(2.0));
        let likes = |id: &str| {
            body["Items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["ItemId"] == id)
                .unwrap()["Likes"]
                .clone()
        };
        assert_eq!(likes(&a), json!(true));
        assert_eq!(likes(&b), json!(false));
    }

    #[tokio::test]
    async fn other_users_ratings_are_excluded() {
        let h = Harness::new().await;
        let other = h
            .second_user()
            .await;
        let a = movie(&h, "A").await;
        let b = movie(&h, "B").await;
        rate(&h, &h.token, &a, 8.0).await;
        rate(&h, &other, &b, 3.0).await;

        let mine_admin = mine(&h, &h.token).await;
        assert_eq!(rating_of(&mine_admin, &a), Some(8.0));
        assert_eq!(rating_of(&mine_admin, &b), None);
        let mine_other = mine(&h, &other).await;
        assert_eq!(rating_of(&mine_other, &b), Some(3.0));
        assert_eq!(rating_of(&mine_other, &a), None);
    }

    #[tokio::test]
    async fn cleared_ratings_and_other_user_state_are_excluded() {
        let h = Harness::new().await;
        let a = movie(&h, "A").await;
        let b = movie(&h, "B").await;
        rate(&h, &h.token, &a, 7.0).await;
        rate(&h, &h.token, &b, 6.0).await;
        // Favouriting creates a state row with no rating: must not appear.
        let c = movie(&h, "C").await;
        h.post_empty(&h.token, &format!("/UserFavoriteItems/{c}"))
            .await
            .assert_status_ok();
        h.delete(&h.token, &format!("/UserItems/{a}/Rating"))
            .await
            .assert_status_ok();

        let body = mine(&h, &h.token).await;
        assert_eq!(rating_of(&body, &a), None);
        assert_eq!(rating_of(&body, &c), None);
        assert_eq!(rating_of(&body, &b), Some(6.0));
        assert_eq!(
            body["Items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn returned_ids_resolve_through_items_ids_lookup() {
        let h = Harness::new().await;
        let a = movie(&h, "A").await;
        let b = movie(&h, "B").await;
        rate(&h, &h.token, &a, 9.0).await;
        rate(&h, &h.token, &b, 4.0).await;

        let body = mine(&h, &h.token).await;
        let ids: Vec<String> = body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                e["ItemId"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        let resp = h
            .get(&h.token, &format!("/Items?ids={}", ids.join(",")))
            .await;
        resp.assert_status_ok();
        let items = resp.json::<Value>();
        let mut got: Vec<String> = items["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                i["Id"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        got.sort();
        let mut want = ids;
        want.sort();
        assert_eq!(got, want);
    }
}
