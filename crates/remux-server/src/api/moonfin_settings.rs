//! Moonfin `Ping` and settings sync (`/Moonfin/Settings/...`).
//!
//! Contract tests only for now; the handlers do not exist yet.

#[cfg(test)]
mod tests {
    use crate::{
        Config, init_app_with_ctx,
        integration_test::auth_header_with_token,
        integration_test::{AUTH_HEADER, TestGuard},
        rewrite_request_uri,
    };
    use axum::Router;
    use axum_test::{TestResponse, TestServer};
    use http::{StatusCode, header};
    use serde_json::{Value, json};
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tower::Layer;
    use tower::util::MapRequestLayer;

    struct Harness {
        server: TestServer,
        app: Router,
        _guard: TestGuard,
        token: String,
        _data: tempfile::TempDir,
    }

    impl Harness {
        fn auth_for(token: &str) -> http::HeaderValue {
            http::HeaderValue::from_str(&auth_header_with_token(token)).unwrap()
        }

        async fn login(&self, name: &str, pw: &str) -> String {
            let resp = self
                .server
                .post("/Users/AuthenticateByName")
                .add_header(
                    header::AUTHORIZATION,
                    http::HeaderValue::from_static(AUTH_HEADER),
                )
                .json(&json!({ "Username": name, "Pw": pw }))
                .await;
            resp.json::<Value>()["AccessToken"]
                .as_str()
                .unwrap()
                .to_string()
        }

        /// Creates another regular user and returns its access token.
        async fn second_user(&self) -> String {
            self.server
                .post("/Users/New")
                .add_header(header::AUTHORIZATION, Self::auth_for(&self.token))
                .json(&json!({ "Name": "second", "Password": "pw" }))
                .await;
            self.login("second", "pw")
                .await
        }

        async fn get(&self, token: &str, path: &str) -> TestResponse {
            self.server
                .get(path)
                .add_header(header::AUTHORIZATION, Self::auth_for(token))
                .await
        }

        async fn post(&self, token: &str, path: &str, body: &Value) -> TestResponse {
            self.server
                .post(path)
                .add_header(header::AUTHORIZATION, Self::auth_for(token))
                .json(body)
                .await
        }

        async fn delete(&self, token: &str, path: &str) -> TestResponse {
            self.server
                .delete(path)
                .add_header(header::AUTHORIZATION, Self::auth_for(token))
                .await
        }

        /// Pushes `profile` the way the Flutter client does.
        async fn push(
            &self,
            token: &str,
            profile: &str,
            settings: Value,
        ) -> TestResponse {
            self.post(
                token,
                &format!("/Moonfin/Settings/Profile/{profile}"),
                &json!({ "profile": settings, "clientId": "moonfin-flutter" }),
            )
            .await
        }

        /// Resolved settings for `profile`, asserting a 200.
        async fn resolved(&self, token: &str, profile: &str) -> Value {
            let resp = self
                .get(
                    token,
                    &format!("/Moonfin/Settings/Resolved/{profile}?client=moonfin"),
                )
                .await;
            resp.assert_status_ok();
            resp.json::<Value>()
        }
    }

    /// Server behind the production URI rewrite layer, so mixed-case Moonfin
    /// URLs reach the lowercase routes (as in `bind_and_serve`).
    async fn harness() -> Harness {
        harness_with(|_, _| {}).await
    }

    async fn harness_with(
        tweak: impl FnOnce(&mut Config, &std::path::Path),
    ) -> Harness {
        let data = tempfile::tempdir().unwrap();
        let mut config = Config {
            database_url: Some("sqlite::memory:".into()),
            torrent_http_port: None,
            torrent_peer_port: None,
            disable_dht: true,
            data_dir: data
                .path()
                .to_path_buf(),
            ..Default::default()
        };
        tweak(&mut config, data.path());
        let (router, ctx) = init_app_with_ctx(config)
            .await
            .unwrap();
        let app = Router::new()
            .fallback_service(MapRequestLayer::new(rewrite_request_uri).layer(router));
        let server = TestServer::builder()
            .mock_transport()
            .build(app.clone())
            .unwrap();
        server
            .post("/Startup/User")
            .json(&json!({ "Name": "test", "Password": "test" }))
            .await;
        server
            .post("/Startup/Complete")
            .await;
        let mut h = Harness {
            server,
            app,
            _guard: TestGuard(ctx),
            token: String::new(),
            _data: data,
        };
        h.token = h
            .login("test", "test")
            .await;
        h
    }

    /// Harness whose admin-defaults file contains `contents` verbatim.
    async fn harness_with_defaults(contents: &str) -> Harness {
        let contents = contents.to_string();
        harness_with(move |config, dir| {
            let path = dir.join("moonfin-defaults.json");
            std::fs::write(&path, contents).unwrap();
            config.moonfin_default_settings_path = Some(
                path.to_string_lossy()
                    .into_owned(),
            );
        })
        .await
    }

    fn normalized_guid(s: &str) -> String {
        s.replace('-', "")
            .to_lowercase()
    }

    // ------------------------------------------------------------------
    // Ping (spec P1-P4)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn ping_requires_authentication() {
        let h = harness().await;
        h.server
            .get("/Moonfin/Ping")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn ping_reports_settings_sync_and_no_optional_features() {
        let h = harness().await;
        let resp = h
            .get(&h.token, "/Moonfin/Ping")
            .await;
        resp.assert_status_ok();
        let content_type = resp
            .header(header::CONTENT_TYPE)
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            content_type.starts_with("application/json"),
            "{content_type}"
        );
        let mut body = resp.json::<Value>();
        let obj = body
            .as_object_mut()
            .unwrap();
        for key in ["version", "serverName"] {
            let value = obj
                .remove(key)
                .unwrap_or_else(|| panic!("missing {key}"));
            assert!(
                value
                    .as_str()
                    .is_some_and(|s| !s.is_empty()),
                "{key} must be a non-empty string"
            );
        }
        assert_eq!(
            body,
            json!({
                "installed": true,
                "settingsSyncEnabled": true,
                "seerrEnabled": false,
                "seerrUrl": null,
                "mdblistAvailable": false,
                "tmdbAvailable": false,
                "tmdbThumbsSupported": false,
                "messagesSupported": false,
                "recommendationsSupported": false,
            })
        );
    }

    #[tokio::test]
    async fn ping_does_not_depend_on_games_and_never_leaks_defaults() {
        // Games are off (default) and an admin defaults file holds a secret.
        let h =
            harness_with_defaults(r#"{"tmdbApiKey":"SECRET","posterSize":"small"}"#)
                .await;
        let resp = h
            .get(&h.token, "/Moonfin/Ping")
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["installed"], json!(true));
        assert_eq!(body["settingsSyncEnabled"], json!(true));
        assert!(
            body.get("defaultSettings")
                .is_none()
        );
        assert!(
            !body
                .to_string()
                .contains("SECRET")
        );
    }

    #[tokio::test]
    async fn ping_routes_with_emby_prefix_and_any_case() {
        let h = harness().await;
        for path in ["/emby/Moonfin/Ping", "/emby/moonfin/ping", "/MOONFIN/PING"] {
            let resp = h
                .get(&h.token, path)
                .await;
            resp.assert_status_ok();
            assert_eq!(resp.json::<Value>()["installed"], json!(true), "{path}");
        }
    }

    // ------------------------------------------------------------------
    // Resolved: basics (E1, E8, S11)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn resolved_requires_authentication() {
        let h = harness().await;
        h.server
            .get("/Moonfin/Settings/Resolved/Mobile")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn resolved_for_user_without_settings_is_empty_object_not_404() {
        let h = harness().await;
        for profile in ["Desktop", "Mobile", "TV", "Global"] {
            assert_eq!(
                h.resolved(&h.token, profile)
                    .await,
                json!({}),
                "{profile}"
            );
        }
    }

    #[tokio::test]
    async fn resolved_rejects_unknown_profile_with_400() {
        let h = harness().await;
        for profile in ["bogus", "phone"] {
            h.get(&h.token, &format!("/Moonfin/Settings/Resolved/{profile}"))
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn profile_names_are_case_insensitive() {
        let h = harness().await;
        h.push(&h.token, "TV", json!({ "a": 1 }))
            .await
            .assert_status_ok();
        for profile in ["tv", "TV", "Tv"] {
            assert_eq!(
                h.resolved(&h.token, profile)
                    .await,
                json!({ "a": 1 }),
                "{profile}"
            );
        }
        // The device profile does not leak into other devices.
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({})
        );
    }

    // ------------------------------------------------------------------
    // Save (S1-S19)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn save_requires_authentication_and_stores_nothing() {
        let h = harness().await;
        h.server
            .post("/Moonfin/Settings/Profile/global")
            .json(&json!({ "profile": { "a": 1 } }))
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({})
        );
    }

    #[tokio::test]
    async fn first_save_reports_success_created_and_user_id() {
        let h = harness().await;
        let me = h
            .get(&h.token, "/Users/Me")
            .await
            .json::<Value>();
        let resp = h
            .push(&h.token, "Global", json!({ "a": 1, "b": "x" }))
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["Success"], json!(true));
        assert_eq!(body["Created"], json!(true));
        assert_eq!(
            normalized_guid(
                body["UserId"]
                    .as_str()
                    .unwrap()
            ),
            normalized_guid(
                me["Id"]
                    .as_str()
                    .unwrap()
            )
        );
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1, "b": "x" })
        );
    }

    #[tokio::test]
    async fn created_is_per_user_not_per_profile() {
        let h = harness().await;
        let first = h
            .push(&h.token, "tv", json!({ "a": 1 }))
            .await
            .json::<Value>();
        let second = h
            .push(&h.token, "desktop", json!({ "a": 2 }))
            .await
            .json::<Value>();
        let third = h
            .push(&h.token, "tv", json!({ "a": 3 }))
            .await
            .json::<Value>();
        assert_eq!(first["Created"], json!(true));
        assert_eq!(second["Created"], json!(false));
        assert_eq!(third["Created"], json!(false));
    }

    #[tokio::test]
    async fn save_merges_top_level_keys_and_keeps_untouched_ones() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1, "b": "x" }))
            .await;
        let resp = h
            .push(&h.token, "global", json!({ "b": "y", "c": true }))
            .await;
        assert_eq!(resp.json::<Value>()["Created"], json!(false));
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1, "b": "y", "c": true })
        );
    }

    #[tokio::test]
    async fn null_values_never_overwrite_stored_ones() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        h.push(&h.token, "global", json!({ "a": null, "b": null }))
            .await
            .assert_status_ok();
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1 })
        );
    }

    #[tokio::test]
    async fn blank_api_keys_are_skipped_but_real_ones_replace() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "tmdbApiKey": "K" }))
            .await;
        for blank in ["", "   ", "null"] {
            h.push(&h.token, "global", json!({ "tmdbApiKey": blank }))
                .await
                .assert_status_ok();
            assert_eq!(
                h.resolved(&h.token, "global")
                    .await,
                json!({ "tmdbApiKey": "K" }),
                "blank {blank:?} must not clear the key"
            );
        }
        h.push(&h.token, "global", json!({ "tmdbApiKey": "K2" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "tmdbApiKey": "K2" })
        );
    }

    #[tokio::test]
    async fn blank_value_for_non_api_key_field_is_stored() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "customThemeId": "x" }))
            .await;
        h.push(&h.token, "global", json!({ "customThemeId": "" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "customThemeId": "" })
        );
    }

    #[tokio::test]
    async fn nested_values_are_replaced_whole_not_deep_merged() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({ "seerrRows": { "trendingMovies": true, "rowOrder": ["a"] } }),
        )
        .await;
        h.push(
            &h.token,
            "global",
            json!({ "seerrRows": { "rowOrder": ["b"] } }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "seerrRows": { "rowOrder": ["b"] } })
        );
    }

    #[tokio::test]
    async fn pushing_row_order_alone_drops_stored_home_sections() {
        let h = harness().await;
        let s1 = json!([{ "type": "resume", "order": 0, "enabled": true }]);
        h.push(
            &h.token,
            "global",
            json!({ "homeSections": s1, "homeRowOrder": ["a"] }),
        )
        .await;
        h.push(&h.token, "global", json!({ "homeRowOrder": ["b"] }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "homeRowOrder": ["b"] })
        );
    }

    #[tokio::test]
    async fn pushing_sections_with_row_order_replaces_both() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({
                "homeSections": [{ "type": "resume", "order": 0, "enabled": true }],
                "homeRowOrder": ["a"],
            }),
        )
        .await;
        let s2 = json!([{ "type": "nextUp", "order": 0, "enabled": true }]);
        h.push(
            &h.token,
            "global",
            json!({ "homeSections": s2, "homeRowOrder": ["b"] }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "homeSections": s2, "homeRowOrder": ["b"] })
        );
    }

    #[tokio::test]
    async fn unrelated_push_keeps_stored_home_layout() {
        let h = harness().await;
        let sections = json!([{ "type": "resume", "order": 0, "enabled": true }]);
        h.push(
            &h.token,
            "global",
            json!({ "homeSections": sections, "homeRowOrder": ["a"] }),
        )
        .await;
        h.push(&h.token, "global", json!({ "posterSize": "s" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({
                "homeSections": sections,
                "homeRowOrder": ["a"],
                "posterSize": "s",
            })
        );
    }

    #[tokio::test]
    async fn saving_a_device_profile_leaves_global_untouched() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        h.push(&h.token, "tv", json!({ "a": 2 }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "a": 2 })
        );
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1 })
        );
    }

    #[tokio::test]
    async fn save_rejects_unknown_profile_name() {
        let h = harness().await;
        h.push(&h.token, "phone", json!({ "a": 1 }))
            .await
            .assert_status(StatusCode::BAD_REQUEST);
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({})
        );
    }

    #[tokio::test]
    async fn save_rejects_missing_null_or_non_object_profile() {
        let h = harness().await;
        for body in [
            json!({ "clientId": "c" }),
            json!({ "profile": null }),
            json!({ "profile": [1, 2] }),
            json!({ "profile": "str" }),
        ] {
            h.post(&h.token, "/Moonfin/Settings/Profile/global", &body)
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({})
        );
    }

    #[tokio::test]
    async fn save_rejects_oversized_body_with_413() {
        let h = harness().await;
        let big = "x".repeat(1024 * 1024 + 1024);
        h.push(&h.token, "global", json!({ "blob": big }))
            .await
            .assert_status(StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({})
        );
    }

    #[tokio::test]
    async fn unknown_keys_are_preserved_verbatim() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({ "zzzFuture": { "n": 1, "list": [true, null] }, "posterSize": "l" }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "zzzFuture": { "n": 1, "list": [true, null] }, "posterSize": "l" })
        );
    }

    #[tokio::test]
    async fn settings_are_isolated_per_user() {
        let h = harness().await;
        let other = h
            .second_user()
            .await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        // The second user has nothing yet, and their first save is "created".
        assert_eq!(
            h.resolved(&other, "global")
                .await,
            json!({})
        );
        let resp = h
            .push(&other, "global", json!({ "a": 9 }))
            .await;
        assert_eq!(resp.json::<Value>()["Created"], json!(true));
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1 })
        );
        assert_eq!(
            h.resolved(&other, "global")
                .await,
            json!({ "a": 9 })
        );
    }

    // ------------------------------------------------------------------
    // Resolve precedence (R1-R15, E2, E3)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn device_profile_overrides_global_for_that_device_only() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "posterSize": "large" }))
            .await;
        h.push(&h.token, "desktop", json!({ "posterSize": "small" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await["posterSize"],
            json!("small")
        );
        assert_eq!(
            h.resolved(&h.token, "mobile")
                .await["posterSize"],
            json!("large")
        );
        assert_eq!(
            h.resolved(&h.token, "global")
                .await["posterSize"],
            json!("large")
        );
    }

    #[tokio::test]
    async fn device_profile_alone_resolves_without_global() {
        let h = harness().await;
        h.push(&h.token, "tv", json!({ "visualTheme": "dark" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "visualTheme": "dark" })
        );
    }

    #[tokio::test]
    async fn keys_missing_from_device_profile_fall_through_to_global() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": "g", "b": "g" }))
            .await;
        h.push(&h.token, "tv", json!({ "b": "tv", "c": "tv" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "a": "g", "b": "tv", "c": "tv" })
        );
    }

    #[tokio::test]
    async fn empty_and_falsy_device_values_win_over_global() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({ "s": "x", "f": true, "n": 5, "l": ["a"], "o": { "k": 1 } }),
        )
        .await;
        h.push(
            &h.token,
            "tv",
            json!({ "s": "", "f": false, "n": 0, "l": [], "o": {} }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "s": "", "f": false, "n": 0, "l": [], "o": {} })
        );
    }

    #[tokio::test]
    async fn arrays_and_objects_resolve_whole_never_merged_across_layers() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({
                "mdblistRatingSources": ["b", "c"],
                "seerrRows": { "trendingMovies": true, "rowOrder": ["y"] },
            }),
        )
        .await;
        h.push(
            &h.token,
            "tv",
            json!({
                "mdblistRatingSources": ["a"],
                "seerrRows": { "rowOrder": ["x"] },
            }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({
                "mdblistRatingSources": ["a"],
                "seerrRows": { "rowOrder": ["x"] },
            })
        );
    }

    #[tokio::test]
    async fn home_row_order_is_derived_from_enabled_sorted_sections() {
        let h = harness().await;
        let sections = json!([
            { "type": "resume", "order": 1, "enabled": true },
            { "type": "nextUp", "order": 0, "enabled": true },
            { "type": "latestMedia", "order": 2, "enabled": false },
            { "type": "x", "kind": "pluginDynamic", "order": 3 },
        ]);
        h.push(&h.token, "global", json!({ "homeSections": sections }))
            .await;
        let resolved = h
            .resolved(&h.token, "global")
            .await;
        assert_eq!(resolved["homeSections"], sections);
        assert_eq!(resolved["homeRowOrder"], json!(["nextUp", "resume"]));
    }

    #[tokio::test]
    async fn home_layout_comes_from_a_single_layer() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({
                "homeSections": [{ "type": "resume", "order": 0, "enabled": true }],
                "homeRowOrder": ["b"],
            }),
        )
        .await;
        h.push(&h.token, "tv", json!({ "homeRowOrder": ["a"] }))
            .await;
        // The device layer wins as a pair, so global's sections are not mixed in.
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "homeRowOrder": ["a"] })
        );

        // A device layer without either key defers to global's pair entirely.
        h.push(&h.token, "mobile", json!({ "posterSize": "s" }))
            .await;
        let mobile = h
            .resolved(&h.token, "mobile")
            .await;
        assert_eq!(mobile["homeRowOrder"], json!(["b"]));
        assert_eq!(
            mobile["homeSections"],
            json!([{ "type": "resume", "order": 0, "enabled": true }])
        );
    }

    #[tokio::test]
    async fn rt_audience_rating_source_is_renamed_for_the_client() {
        let h = harness().await;
        h.push(
            &h.token,
            "global",
            json!({ "mdblistRatingSources": ["rtAudience", "imdb"] }),
        )
        .await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await["mdblistRatingSources"],
            json!(["tomatoes_audience", "imdb"])
        );
    }

    // ------------------------------------------------------------------
    // Deletes (E4-E7)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn deletes_require_authentication() {
        let h = harness().await;
        h.server
            .delete("/Moonfin/Settings")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        h.server
            .delete("/Moonfin/Settings/Profile/desktop")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn deleting_a_device_profile_falls_back_to_global() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "posterSize": "large" }))
            .await;
        h.push(&h.token, "desktop", json!({ "posterSize": "small" }))
            .await;
        let resp = h
            .delete(&h.token, "/Moonfin/Settings/Profile/Desktop")
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<Value>()["Success"], json!(true));
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({ "posterSize": "large" })
        );
    }

    #[tokio::test]
    async fn deleting_global_or_unknown_profile_is_a_400_and_changes_nothing() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        for profile in ["global", "Global", "nope"] {
            h.delete(&h.token, &format!("/Moonfin/Settings/Profile/{profile}"))
                .await
                .assert_status(StatusCode::BAD_REQUEST);
        }
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "a": 1 })
        );
    }

    #[tokio::test]
    async fn deleting_a_missing_profile_succeeds() {
        let h = harness().await;
        h.delete(&h.token, "/Moonfin/Settings/Profile/tv")
            .await
            .assert_status_ok();
    }

    #[tokio::test]
    async fn delete_all_clears_only_that_users_settings_and_is_idempotent() {
        let h = harness().await;
        let other = h
            .second_user()
            .await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        h.push(&h.token, "tv", json!({ "a": 2 }))
            .await;
        h.push(&other, "global", json!({ "a": 9 }))
            .await;

        let resp = h
            .delete(&h.token, "/Moonfin/Settings")
            .await;
        resp.assert_status_ok();
        assert_eq!(resp.json::<Value>()["Success"], json!(true));
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({})
        );
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({})
        );
        assert_eq!(
            h.resolved(&other, "global")
                .await,
            json!({ "a": 9 })
        );

        // Nothing left to delete is still a success.
        h.delete(&h.token, "/Moonfin/Settings")
            .await
            .assert_status_ok();
        // And the user can save again afterwards, as a first save.
        let again = h
            .push(&h.token, "global", json!({ "a": 3 }))
            .await;
        assert_eq!(again.json::<Value>()["Created"], json!(true));
    }

    // ------------------------------------------------------------------
    // Admin defaults file (R5, R6, R13, R14, R16, R17)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn admin_defaults_are_served_to_users_without_settings() {
        let h =
            harness_with_defaults(r#"{"posterSize":"small","navbarPosition":"top"}"#)
                .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({ "posterSize": "small", "navbarPosition": "top" })
        );
    }

    #[tokio::test]
    async fn admin_defaults_are_the_lowest_precedence_layer() {
        let h = harness_with_defaults(
            r#"{"posterSize":"small","navbarPosition":"top","x":1}"#,
        )
        .await;
        h.push(&h.token, "global", json!({ "posterSize": "large" }))
            .await;
        h.push(&h.token, "tv", json!({ "navbarPosition": "left" }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "tv")
                .await,
            json!({ "posterSize": "large", "navbarPosition": "left", "x": 1 })
        );
        assert_eq!(
            h.resolved(&h.token, "mobile")
                .await,
            json!({ "posterSize": "large", "navbarPosition": "top", "x": 1 })
        );
    }

    #[tokio::test]
    async fn admin_defaults_null_values_are_absent() {
        let h = harness_with_defaults(r#"{"x":null,"posterSize":"small"}"#).await;
        assert_eq!(
            h.resolved(&h.token, "global")
                .await,
            json!({ "posterSize": "small" })
        );
    }

    #[tokio::test]
    async fn admin_defaults_home_layout_is_a_pair_and_never_mixes_with_user_layers() {
        let defaults = r#"{"homeSections":[{"type":"none","order":0,"enabled":true}]}"#;
        let h = harness_with_defaults(defaults).await;
        // Only the defaults layer has a layout: sections echoed, derived order empty.
        let resolved = h
            .resolved(&h.token, "desktop")
            .await;
        assert_eq!(
            resolved,
            json!({ "homeSections": [{ "type": "none", "order": 0, "enabled": true }] })
        );

        // A user layer with a row order wins as a pair, so the default
        // sections do not leak in next to it.
        h.push(&h.token, "global", json!({ "homeRowOrder": ["b"] }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({ "homeRowOrder": ["b"] })
        );
    }

    #[tokio::test]
    async fn missing_admin_defaults_file_behaves_as_no_defaults() {
        let h = harness_with(|config, dir| {
            config.moonfin_default_settings_path = Some(
                dir.join("does-not-exist.json")
                    .to_string_lossy()
                    .into_owned(),
            );
        })
        .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({})
        );
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        assert_eq!(
            h.resolved(&h.token, "desktop")
                .await,
            json!({ "a": 1 })
        );
    }

    #[tokio::test]
    async fn invalid_admin_defaults_file_behaves_as_no_defaults() {
        for contents in ["not json at all", "[1,2,3]", "\"str\""] {
            let h = harness_with_defaults(contents).await;
            assert_eq!(
                h.resolved(&h.token, "desktop")
                    .await,
                json!({}),
                "{contents}"
            );
        }
    }

    // ------------------------------------------------------------------
    // Seerr stub: plugin is "installed" but Seerr is not configured
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn seerr_config_reports_disabled() {
        let h = harness().await;
        h.server
            .get("/Moonfin/Seerr/Config")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        let resp = h
            .get(&h.token, "/Moonfin/Seerr/Config")
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["enabled"], json!(false));
        // Absent or false: the client must not turn Seerr on for this user.
        assert!(
            body.get("userEnabled")
                .is_none_or(|v| v == &json!(false))
        );
    }

    #[tokio::test]
    async fn seerr_status_reports_disabled_so_login_bootstrap_returns_at_once() {
        // The client's post-login Seerr bootstrap calls this and returns
        // without a sign-in attempt when `enabled` is false.
        let h = harness().await;
        h.server
            .get("/Moonfin/Seerr/Status")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
        let resp = h
            .get(&h.token, "/Moonfin/Seerr/Status")
            .await;
        resp.assert_status_ok();
        let body = resp.json::<Value>();
        assert_eq!(body["enabled"], json!(false));
        assert_eq!(body["authenticated"], json!(false));
    }

    // ------------------------------------------------------------------
    // SSE stream (SSE1-SSE3, SSE5, SSE6)
    // ------------------------------------------------------------------

    /// Minimal raw-HTTP SSE reader over a real socket (the mock transport
    /// buffers whole bodies, which an open stream never finishes).
    struct SseConn {
        stream: TcpStream,
        seen: String,
    }

    impl SseConn {
        async fn open(app: &Router, token: &str) -> SseConn {
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .unwrap();
            let addr = listener
                .local_addr()
                .unwrap();
            let app = app.clone();
            tokio::spawn(async move {
                axum::serve(
                    listener,
                    app.into_make_service_with_connect_info::<SocketAddr>(),
                )
                .await
                .ok();
            });
            let mut stream = TcpStream::connect(addr)
                .await
                .unwrap();
            let request = format!(
                "GET /Moonfin/Settings/Stream HTTP/1.1\r\nHost: localhost\r\n\
                 Accept: text/event-stream\r\nAuthorization: {}\r\n\r\n",
                auth_header_with_token(token)
            );
            stream
                .write_all(request.as_bytes())
                .await
                .unwrap();
            SseConn {
                stream,
                seen: String::new(),
            }
        }

        async fn read_some(&mut self, wait: Duration) {
            let mut buf = [0u8; 4096];
            if let Ok(Ok(n)) = tokio::time::timeout(
                wait,
                self.stream
                    .read(&mut buf),
            )
            .await
            {
                self.seen
                    .push_str(&String::from_utf8_lossy(&buf[..n]));
            }
        }

        async fn wait_for(&mut self, needle: &str) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            while !self
                .seen
                .contains(needle)
            {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "timed out waiting for {needle:?}; saw {:?}",
                    self.seen
                );
                self.read_some(Duration::from_millis(200))
                    .await;
            }
        }

        /// True if `needle` shows up within `wait`.
        async fn appears_within(&mut self, needle: &str, wait: Duration) -> bool {
            let deadline = tokio::time::Instant::now() + wait;
            while tokio::time::Instant::now() < deadline {
                if self
                    .seen
                    .contains(needle)
                {
                    return true;
                }
                self.read_some(Duration::from_millis(50))
                    .await;
            }
            self.seen
                .contains(needle)
        }
    }

    const EVENT: &str = r#"data: {"type":"settingsUpdated"}"#;
    const QUIET: Duration = Duration::from_millis(600);

    #[tokio::test]
    async fn stream_requires_authentication() {
        let h = harness().await;
        h.server
            .get("/Moonfin/Settings/Stream")
            .await
            .assert_status(StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn stream_is_event_stream_and_announces_itself_immediately() {
        let h = harness().await;
        let mut sse = SseConn::open(&h.app, &h.token).await;
        sse.wait_for(":connected")
            .await;
        let head = sse
            .seen
            .to_lowercase();
        assert!(head.starts_with("http/1.1 200"), "{head}");
        assert!(head.contains("content-type: text/event-stream"), "{head}");
    }

    #[tokio::test]
    async fn changed_save_notifies_the_same_users_stream_only() {
        let h = harness().await;
        let other = h
            .second_user()
            .await;
        let mut mine = SseConn::open(&h.app, &h.token).await;
        let mut theirs = SseConn::open(&h.app, &other).await;
        mine.wait_for(":connected")
            .await;
        theirs
            .wait_for(":connected")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        h.push(&h.token, "tv", json!({ "a": 1 }))
            .await
            .assert_status_ok();

        mine.wait_for(EVENT)
            .await;
        assert!(
            !theirs
                .appears_within(EVENT, QUIET)
                .await,
            "other user's stream must stay quiet: {:?}",
            theirs.seen
        );
    }

    #[tokio::test]
    async fn saving_an_identical_payload_does_not_notify_again() {
        let h = harness().await;
        let mut sse = SseConn::open(&h.app, &h.token).await;
        sse.wait_for(":connected")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        h.push(&h.token, "tv", json!({ "a": 1 }))
            .await;
        sse.wait_for(EVENT)
            .await;
        let before = sse
            .seen
            .matches(EVENT)
            .count();

        h.push(&h.token, "tv", json!({ "a": 1 }))
            .await
            .assert_status_ok();
        let _ = sse
            .appears_within("never-appears", QUIET)
            .await;
        assert_eq!(
            sse.seen
                .matches(EVENT)
                .count(),
            before,
            "{:?}",
            sse.seen
        );
    }

    #[tokio::test]
    async fn deletes_do_not_notify_the_stream() {
        let h = harness().await;
        h.push(&h.token, "global", json!({ "a": 1 }))
            .await;
        h.push(&h.token, "tv", json!({ "a": 2 }))
            .await;
        let mut sse = SseConn::open(&h.app, &h.token).await;
        sse.wait_for(":connected")
            .await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        h.delete(&h.token, "/Moonfin/Settings/Profile/tv")
            .await
            .assert_status_ok();
        h.delete(&h.token, "/Moonfin/Settings")
            .await
            .assert_status_ok();
        assert!(
            !sse.appears_within(EVENT, QUIET)
                .await,
            "{:?}",
            sse.seen
        );
    }
}
