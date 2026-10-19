//! Moonfin per-user collection ordering
//! (`GET|POST /Moonfin/Collections/{collectionId}/Order`).
//!
//! Wire behaviour follows Moonbase's `CollectionsController`: the body and
//! response are a bare JSON array of item id strings. The client reads it to
//! force a custom sort (a non-empty list wins over release-date order) and
//! writes the whole flat order, episode ids included, after a drag-reorder.
//!
//! * `GET` returns the stored array, or `[]` when nothing is stored. That
//!   also holds for unknown collections, matching Moonbase.
//! * `POST` replaces the order and answers `204`; `[]` clears it. Ids are
//!   stored verbatim (dashless or hyphenated, whatever the client sent) so
//!   `GET` echoes exactly what was saved.
//! * `POST` of a non-empty order requires `collectionId` to be an existing
//!   collection the user's policy lets them see (`404` otherwise), and a user
//!   holds at most `MAX_ORDERS_PER_USER` orders and `MAX_ORDER_BYTES` of
//!   stored order JSON in total (`413`). Clearing (`[]`) is
//!   always allowed so stale rows for deleted collections can be removed.
//! * Orders are per user and independent of the native, admin-only,
//!   global `move` endpoint.

use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use remux_macros::{get, post};
use uuid::Uuid;

use crate::{
    AppState,
    api::items::apply_user_policy,
    api::moonfin_settings::{error_response, internal_error},
    db::{self, auth::AuthSession},
};

/// Most ids accepted in one order.
const MAX_IDS: usize = 20_000;
/// Most collections one user may hold a stored order for.
const MAX_ORDERS_PER_USER: i64 = 1000;
/// Most stored order JSON bytes one user may hold across all collections.
const MAX_ORDER_BYTES: i64 = 8 * 1024 * 1024;
/// Largest accepted body (a full list of dashed UUIDs is ~750 KB).
const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// A saved collection order: bounded, and every entry a valid item id.
#[derive(Debug, PartialEq)]
struct CollectionOrder(Vec<String>);

impl CollectionOrder {
    fn parse(body: &[u8]) -> Result<Self, String> {
        if body.len() > MAX_BODY_BYTES {
            return Err("order body too large".into());
        }
        let ids: Vec<String> = serde_json::from_slice(body)
            .map_err(|_| "body must be a JSON array of item id strings".to_string())?;
        if ids.len() > MAX_IDS {
            return Err(format!("order holds more than {MAX_IDS} ids"));
        }
        if let Some(bad) = ids
            .iter()
            .find(|id| Uuid::parse_str(id).is_err())
        {
            return Err(format!("invalid item id: {bad}"));
        }
        Ok(Self(ids))
    }
}

/// Whether `collection_id` is a collection the session user may see, with
/// the policy's `CollectionId` hide/allow rules applied like browse views do.
async fn collection_visible(
    state: &AppState,
    session: &AuthSession,
    collection_id: Uuid,
) -> anyhow::Result<bool> {
    let mut filter = db::MediaFilter {
        id: Some(vec![collection_id]),
        kind: Some(vec![db::MediaKind::Collection]),
        limit: Some(1),
        ..Default::default()
    };
    apply_user_policy(&mut filter, &session.user);
    let found = db::Media::get_by_filter(
        &state
            .ctx
            .db,
        &filter,
    )
    .await?;
    // Container rows are exempt from the SQL rating filter (see AGENTS.md), so
    // a collection that carries its own certification is checked here.
    let max_rating = filter.max_parental_rating;
    Ok(found
        .records
        .first()
        .is_some_and(|c| {
            max_rating
                .zip(c.certification_age)
                .is_none_or(|(max, age)| age <= max)
        }))
}

#[get("/moonfin/collections/{collection_id}/order")]
pub async fn moonfin_collection_order_get(
    State(state): State<AppState>,
    session: AuthSession,
    Path(collection_id): Path<Uuid>,
) -> Response {
    let stored: Result<Option<String>, _> = sqlx::query_scalar(
        "SELECT item_ids FROM moonfin_collection_order \
         WHERE user_id = ?1 AND collection_id = ?2",
    )
    .bind(
        session
            .user
            .id,
    )
    .bind(collection_id)
    .fetch_optional(
        &state
            .ctx
            .db,
    )
    .await;
    match stored {
        Ok(Some(json)) => (
            StatusCode::OK,
            [(http::header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response(),
        Ok(None) => Json(Vec::<String>::new()).into_response(),
        Err(e) => internal_error(e),
    }
}

#[post("/moonfin/collections/{collection_id}/order")]
pub async fn moonfin_collection_order_save(
    State(state): State<AppState>,
    session: AuthSession,
    Path(collection_id): Path<Uuid>,
    body: Bytes,
) -> Response {
    let order = match CollectionOrder::parse(&body) {
        Ok(order) => order,
        Err(msg) => return error_response(StatusCode::BAD_REQUEST, msg),
    };
    let user_id = session
        .user
        .id;
    let db = &state
        .ctx
        .db;
    if !order
        .0
        .is_empty()
    {
        match collection_visible(&state, &session, collection_id).await {
            Ok(true) => {}
            Ok(false) => {
                return error_response(StatusCode::NOT_FOUND, "Collection not found");
            }
            Err(e) => return internal_error(e),
        }
    }
    let result = if order
        .0
        .is_empty()
    {
        sqlx::query(
            "DELETE FROM moonfin_collection_order \
             WHERE user_id = ?1 AND collection_id = ?2",
        )
        .bind(user_id)
        .bind(collection_id)
        .execute(db)
        .await
    } else {
        sqlx::query(
            "INSERT INTO moonfin_collection_order \
             (user_id, collection_id, item_ids, updated_at) \
             SELECT ?1, ?2, ?3, ?4 \
             WHERE (EXISTS (SELECT 1 FROM moonfin_collection_order \
                           WHERE user_id = ?1 AND collection_id = ?2) \
                OR (SELECT COUNT(*) FROM moonfin_collection_order \
                    WHERE user_id = ?1) < ?5) \
             AND (SELECT COALESCE(SUM(LENGTH(item_ids)), 0) \
                  FROM moonfin_collection_order \
                  WHERE user_id = ?1 AND collection_id <> ?2) \
                 + LENGTH(?3) <= ?6 \
             ON CONFLICT (user_id, collection_id) DO UPDATE SET \
             item_ids = excluded.item_ids, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(collection_id)
        .bind(serde_json::to_string(&order.0).unwrap_or_else(|_| "[]".into()))
        .bind(chrono::Utc::now().timestamp_millis())
        .bind(MAX_ORDERS_PER_USER)
        .bind(MAX_ORDER_BYTES)
        .execute(db)
        .await
    };
    match result {
        // The guarded insert wrote nothing: a per-user cap was hit.
        Ok(done)
            if done.rows_affected() == 0
                && !order
                    .0
                    .is_empty() =>
        {
            error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "at most {MAX_ORDERS_PER_USER} collection orders and {} MiB of order data per user",
                    MAX_ORDER_BYTES / (1024 * 1024)
                ),
            )
        }
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::moonfin_test_support::Harness;
    use serde_json::{Value, json};

    const COL: &str = "0b7c1e2a9f3d5a6b8c7d112233445566";
    const COL2: &str = "1b7c1e2a9f3d5a6b8c7d112233445566";

    /// Harness with COL and COL2 seeded as real collections.
    async fn harness() -> Harness {
        let h = Harness::new().await;
        seed_collection(&h, COL).await;
        seed_collection(&h, COL2).await;
        h
    }

    async fn seed_collection(h: &Harness, id: &str) {
        db::Media {
            id: Uuid::parse_str(id).unwrap(),
            title: format!("collection {id}"),
            kind: db::MediaKind::Collection,
            collection_kind: Some(db::CollectionKind::Manual),
            ..Default::default()
        }
        .save(
            &h.ctx
                .db,
        )
        .await
        .unwrap();
    }

    fn ids(n: usize) -> Vec<String> {
        (1..=n)
            .map(|i| {
                Uuid::from_u128(i as u128)
                    .simple()
                    .to_string()
            })
            .collect()
    }

    fn path(col: &str) -> String {
        format!("/Moonfin/Collections/{col}/Order")
    }

    async fn order(h: &Harness, token: &str, col: &str) -> Value {
        let resp = h
            .get(token, &path(col))
            .await;
        resp.assert_status_ok();
        resp.json::<Value>()
    }

    #[tokio::test]
    async fn requires_authentication() {
        let h = Harness::new().await;
        h.server
            .get(&path(COL))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        h.server
            .post(&path(COL))
            .json(&ids(2))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        assert_eq!(order(&h, &h.token, COL).await, json!([]));
    }

    #[tokio::test]
    async fn unset_order_is_an_empty_array_even_for_unknown_collections() {
        let h = Harness::new().await;
        assert_eq!(order(&h, &h.token, COL).await, json!([]));
    }

    #[tokio::test]
    async fn saved_order_round_trips_in_order_and_verbatim() {
        let h = harness().await;
        let mut sent = ids(3);
        sent.reverse();
        // One hyphenated id: stored in the form the client sent.
        sent[1] = Uuid::from_u128(2).to_string();
        let resp = h
            .post(&h.token, &path(COL), &json!(sent))
            .await;
        resp.assert_status(StatusCode::NO_CONTENT);
        assert_eq!(order(&h, &h.token, COL).await, json!(sent));
    }

    #[tokio::test]
    async fn collection_id_form_does_not_matter() {
        let h = harness().await;
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let hyphenated = Uuid::parse_str(COL)
            .unwrap()
            .to_string();
        assert_eq!(order(&h, &h.token, &hyphenated).await, json!(ids(2)));
    }

    #[tokio::test]
    async fn orders_are_per_user() {
        let h = harness().await;
        let other = h
            .second_user()
            .await;
        h.post(&h.token, &path(COL), &json!(ids(3)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(order(&h, &other, COL).await, json!([]));

        let mut theirs = ids(3);
        theirs.reverse();
        h.post(&other, &path(COL), &json!(theirs))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(order(&h, &h.token, COL).await, json!(ids(3)));
        assert_eq!(order(&h, &other, COL).await, json!(theirs));
    }

    #[tokio::test]
    async fn collections_are_independent_and_save_overwrites() {
        let h = harness().await;
        h.post(&h.token, &path(COL), &json!(ids(3)))
            .await;
        h.post(&h.token, &path(COL2), &json!(ids(1)))
            .await;
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(order(&h, &h.token, COL).await, json!(ids(2)));
        assert_eq!(order(&h, &h.token, COL2).await, json!(ids(1)));
    }

    #[tokio::test]
    async fn empty_array_clears_the_order() {
        let h = harness().await;
        h.post(&h.token, &path(COL), &json!(ids(3)))
            .await;
        h.post(&h.token, &path(COL), &json!([]))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(order(&h, &h.token, COL).await, json!([]));
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM moonfin_collection_order")
                .fetch_one(
                    &h.ctx
                        .db,
                )
                .await
                .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn invalid_bodies_are_400_and_store_nothing() {
        let h = harness().await;
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await;
        for bad in [
            json!({ "ids": ["a"] }),
            json!("not an array"),
            json!([1, 2]),
            json!([null]),
            json!(["not-a-guid"]),
            json!(
                ids(2)
                    .into_iter()
                    .chain([String::new()])
                    .collect::<Vec<_>>()
            ),
        ] {
            h.post(&h.token, &path(COL), &bad)
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
        let too_many: Vec<String> = ids(1)
            .into_iter()
            .cycle()
            .take(MAX_IDS + 1)
            .collect();
        h.post(&h.token, &path(COL), &json!(too_many))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        // Prior order untouched by every rejected save.
        assert_eq!(order(&h, &h.token, COL).await, json!(ids(2)));
    }

    #[tokio::test]
    async fn deleting_a_user_removes_their_orders() {
        let h = harness().await;
        let other = h
            .second_user()
            .await;
        h.post(&other, &path(COL), &json!(ids(2)))
            .await;
        let resp = h
            .get(&h.token, "/Users")
            .await;
        let users = resp.json::<Value>();
        let second = users
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["Name"] == "second")
            .unwrap()["Id"]
            .as_str()
            .unwrap()
            .to_string();
        h.delete(&h.token, &format!("/Users/{second}"))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM moonfin_collection_order")
                .fetch_one(
                    &h.ctx
                        .db,
                )
                .await
                .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn saving_an_order_for_an_unknown_collection_is_404_and_stores_nothing() {
        let h = Harness::new().await;
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NOT_FOUND);
        assert_eq!(order(&h, &h.token, COL).await, json!([]));
        assert_eq!(row_count(&h).await, 0);
    }

    #[tokio::test]
    async fn saving_an_order_for_a_non_collection_item_is_404() {
        let h = Harness::new().await;
        let movie =
            crate::api::moonfin_test_support::seed_movie(&h.ctx, "M", 9101).await;
        h.post(&h.token, &path(&movie), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NOT_FOUND);
        assert_eq!(row_count(&h).await, 0);
    }

    #[tokio::test]
    async fn clearing_an_order_for_a_missing_collection_is_allowed() {
        let h = Harness::new().await;
        sqlx::query(
            "INSERT INTO moonfin_collection_order VALUES (\
             (SELECT id FROM users LIMIT 1), ?1, '[]', 0)",
        )
        .bind(Uuid::parse_str(COL).unwrap())
        .execute(
            &h.ctx
                .db,
        )
        .await
        .unwrap();
        h.post(&h.token, &path(COL), &json!([]))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(row_count(&h).await, 0);
    }

    #[tokio::test]
    async fn collection_hidden_by_the_users_policy_is_404() {
        let h = harness().await;
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
        policy["FilterRules"] =
            serde_json::to_value(remux_sdks::remux::CollectionFilter {
                groups: vec![remux_sdks::remux::FilterGroup {
                    rules: vec![remux_sdks::remux::FilterRule::CollectionId {
                        op: remux_sdks::remux::SetOp::IsNot,
                        ids: vec![Uuid::parse_str(COL).unwrap()],
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            })
            .unwrap();
        h.post(&h.token, &format!("/Users/{id}/Policy"), &policy)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        h.post(&kid, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NOT_FOUND);
        assert_eq!(order(&h, &kid, COL).await, json!([]));
        // Other collections, and the unrestricted admin, are unaffected.
        h.post(&kid, &path(COL2), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn orders_per_user_are_capped_but_existing_rows_stay_updatable() {
        let h = harness().await;
        let user: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
            .fetch_one(
                &h.ctx
                    .db,
            )
            .await
            .unwrap();
        // Fill the quota with stale rows directly (cheaper than 1000 collections).
        for i in 0..MAX_ORDERS_PER_USER {
            sqlx::query(
                "INSERT INTO moonfin_collection_order VALUES (?1, ?2, '[]', 0)",
            )
            .bind(user)
            .bind(Uuid::from_u128(1_000_000 + i as u128))
            .execute(
                &h.ctx
                    .db,
            )
            .await
            .unwrap();
        }
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(order(&h, &h.token, COL).await, json!([]));
        // Another user has their own quota.
        let other = h
            .second_user()
            .await;
        h.post(&other, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        // Freeing a slot lets the save through, and then it can be rewritten.
        sqlx::query("DELETE FROM moonfin_collection_order WHERE collection_id = ?1")
            .bind(Uuid::from_u128(1_000_000))
            .execute(
                &h.ctx
                    .db,
            )
            .await
            .unwrap();
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        h.post(&h.token, &path(COL), &json!(ids(3)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        assert_eq!(order(&h, &h.token, COL).await, json!(ids(3)));
    }

    #[tokio::test]
    async fn stored_order_bytes_are_capped_per_user_but_a_row_can_shrink() {
        let h = harness().await;
        let user: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
            .fetch_one(
                &h.ctx
                    .db,
            )
            .await
            .unwrap();
        // A stale row for another collection fills the whole byte quota.
        sqlx::query(
            "INSERT INTO moonfin_collection_order VALUES \
             (?1, ?2, REPLACE(HEX(ZEROBLOB(?3)), '0', 'a'), 0)",
        )
        .bind(user)
        .bind(Uuid::from_u128(1_000_000))
        .bind(MAX_ORDER_BYTES / 2)
        .execute(
            &h.ctx
                .db,
        )
        .await
        .unwrap();
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(order(&h, &h.token, COL).await, json!([]));
        // Another user has their own byte quota.
        let other = h
            .second_user()
            .await;
        h.post(&other, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        // Shrinking the stale row frees room.
        sqlx::query("UPDATE moonfin_collection_order SET item_ids = '[]' WHERE collection_id = ?1")
            .bind(Uuid::from_u128(1_000_000))
            .execute(
                &h.ctx
                    .db,
            )
            .await
            .unwrap();
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn collection_rated_above_the_users_limit_is_404() {
        let h = harness().await;
        db::Media {
            id: Uuid::parse_str(COL).unwrap(),
            title: "adult".into(),
            kind: db::MediaKind::Collection,
            collection_kind: Some(db::CollectionKind::Manual),
            certification_age: Some(18),
            ..Default::default()
        }
        .save(
            &h.ctx
                .db,
        )
        .await
        .unwrap();
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
        policy["MaxParentalRating"] = json!(10);
        h.post(&h.token, &format!("/Users/{id}/Policy"), &policy)
            .await
            .assert_status(StatusCode::NO_CONTENT);

        h.post(&kid, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NOT_FOUND);
        // An unrated collection and the unrestricted admin are unaffected.
        h.post(&kid, &path(COL2), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
        h.post(&h.token, &path(COL), &json!(ids(2)))
            .await
            .assert_status(StatusCode::NO_CONTENT);
    }

    async fn row_count(h: &Harness) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM moonfin_collection_order")
            .fetch_one(
                &h.ctx
                    .db,
            )
            .await
            .unwrap()
    }
}
