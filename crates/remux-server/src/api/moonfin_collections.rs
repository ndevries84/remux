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
    api::moonfin_settings::{error_response, internal_error},
    db::auth::AuthSession,
};

/// Most ids accepted in one order.
const MAX_IDS: usize = 20_000;
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
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (user_id, collection_id) DO UPDATE SET \
             item_ids = excluded.item_ids, updated_at = excluded.updated_at",
        )
        .bind(user_id)
        .bind(collection_id)
        .bind(serde_json::to_string(&order.0).unwrap_or_else(|_| "[]".into()))
        .bind(chrono::Utc::now().timestamp_millis())
        .execute(db)
        .await
    };
    match result {
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
        let h = Harness::new().await;
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
        let h = Harness::new().await;
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
        let h = Harness::new().await;
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
        let h = Harness::new().await;
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
        let h = Harness::new().await;
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
        let h = Harness::new().await;
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
        let h = Harness::new().await;
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
}
