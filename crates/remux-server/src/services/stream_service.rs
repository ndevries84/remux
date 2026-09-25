use crate::{
    AppContext, api,
    conversions::apply_filename_guess,
    db,
    db::PreProbeQualityExt,
    device_profile::{CodecProfileExt, DirectPlayProfileExt, ProfileConditionExt},
    playback::probe::{ProbeDataExt, probe_stream, resolve_stream_root},
};
use remux_sdks::{
    remux::{
        CodecProfileType, MediaStreamType, ProfileConditionProperty, StreamFilter,
        VideoRangeType,
    },
    remuxdb,
};
use tracing::debug;
use uuid::Uuid;

/// Result of probing a single stream candidate.
pub(crate) struct ProbeResult {
    /// Probed source info with id/name/path/remux already stamped.
    pub source: api::MediaSourceInfo,
    /// Original candidate stream (needed for RTSP check, subtitle extraction).
    pub stream: db::Media,
    /// Effective stream post-fallback (may differ from `stream` if probe failed over).
    pub effective_stream: db::Media,
}

/// Result of `StreamService::probe_candidates`.
pub(crate) struct ProbedStreams {
    pub results: Vec<ProbeResult>,
    /// True when the client named a specific stream — keep its UUID, don't override to item_id.
    pub specific_requested: bool,
}

pub(crate) struct StreamServiceConfig {
    pub ctx: AppContext,
    pub item_id: Uuid,
    pub requested_id: Option<Uuid>,
    pub show_ungrouped: bool,
    pub stream_filter: Option<StreamFilter>,
    pub user_id: Option<Uuid>,
}

/// Central service for stream selection on a single playback request.
///
/// Construct with `new()`, then call `resolve()` to do all async work (group detection,
/// stream loading, policy filtering). After that the selection and ID-mapping methods
/// are available with no further parameters.
pub(crate) struct StreamService {
    ctx: AppContext,
    pub item_id: Uuid,
    pub requested_id: Option<Uuid>,
    show_ungrouped: bool,
    stream_filter: Option<StreamFilter>,
    user_id: Option<Uuid>,
    // Populated by resolve()
    group: Option<(Uuid, String, Vec<db::Media>)>,
    stream: Option<db::Media>,
    pub streams: Vec<db::Media>,
    /// Requesting client's device profile, when known. Drives device-aware
    /// pre-probe ordering in the auto-play (`MediaSourceId == item_id`)
    /// branch of `select_streams` — see `device_aware_probe_pool`. `None`
    /// leaves that branch's behaviour unchanged.
    pub(crate) device_profile: Option<api::DeviceProfile>,
    /// Bitrate cap to weigh alongside `device_profile` in the same branch.
    pub(crate) max_bitrate: Option<u64>,
}

fn quality_ordered_probe_pool(streams: &[db::Media]) -> Vec<db::Media> {
    let mut pool = streams.to_vec();
    pool.sort_by_cached_key(|stream| std::cmp::Reverse(stream.quality_weight()));
    pool
}

/// True when `stream`'s probed-or-filename-guessed video codec, range and
/// estimated bitrate can all be satisfied by `profile`/`max_bitrate`:
///
/// (a) the video codec appears in any `DirectPlayProfile.VideoCodec` list
///     (container is not considered here — that's a separate, independent
///     direct-play concern this pre-probe heuristic isn't trying to answer);
/// (b) if a video `CodecProfile` for that codec carries a `VideoRangeType`
///     condition, the candidate's range (unknown ⇒ compatible) satisfies it;
/// (c) if `max_bitrate` is given and an estimated bitrate is known, it is at
///     or under the cap (unknown ⇒ compatible).
///
/// Reuses the same `MediaSourceInfo::from(db::Media)` + `apply_filename_guess`
/// conversion the real playback/ranking path uses, so "probe data if present
/// else filename guess" is exactly the engine's own fallback, not a
/// reimplementation of it — and the same `DirectPlayProfileExt`/
/// `CodecProfileExt`/`ProfileConditionExt` helpers `check_direct_play` itself
/// is built from.
fn device_aware_compatible(
    stream: &db::Media,
    profile: &api::DeviceProfile,
    max_bitrate: Option<u64>,
) -> bool {
    let mut info = api::MediaSourceInfo::from(stream.clone());
    apply_filename_guess(&mut info, stream);

    let Some(codec) = info
        .video_stream()
        .and_then(|v| {
            v.codec
                .as_deref()
        })
    else {
        // No usable video codec signal even after a filename guess: nothing
        // to disqualify the candidate on, so treat it as compatible.
        return true;
    };

    let codec_ok = profile
        .direct_play_profiles
        .iter()
        .any(|p| p.supports_video_codec(codec));
    if !codec_ok {
        return false;
    }

    let range = info
        .video_stream()
        .and_then(|v| {
            v.video_range_type
                .as_ref()
        })
        .map(|r| r.as_str());
    let range_ok = profile
        .codec_profiles
        .iter()
        .filter(|cp| matches!(cp.type_, None | Some(CodecProfileType::Video)))
        .filter(|cp| cp.applies_to_codec(codec))
        .flat_map(|cp| &cp.conditions)
        .filter(|cond| {
            cond.property
                .as_ref()
                == Some(&ProfileConditionProperty::VideoRangeType)
        })
        .all(|cond| cond.is_satisfied_opt(range));
    if !range_ok {
        return false;
    }

    if let Some(max) = max_bitrate {
        if let Some(bitrate) = info.bitrate {
            if bitrate > 0 && bitrate as u64 > max {
                return false;
            }
        }
    }

    true
}

/// Device-aware pre-probe ordering (lostb1t/remux#552): with `profile ==
/// None` this is byte-for-byte `quality_ordered_probe_pool(streams)` — pure
/// filename-derived quality order, unchanged from today. With a profile, the
/// same quality-ordered pool is stably partitioned into compatible-first
/// (per `device_aware_compatible`) then everything else, each half keeping
/// its existing quality order.
pub(crate) fn device_aware_probe_pool(
    streams: &[db::Media],
    profile: Option<&api::DeviceProfile>,
    max_bitrate: Option<u64>,
) -> Vec<db::Media> {
    let pool = quality_ordered_probe_pool(streams);
    let Some(profile) = profile else {
        return pool;
    };
    let (compatible, rest): (Vec<db::Media>, Vec<db::Media>) = pool
        .into_iter()
        .partition(|stream| device_aware_compatible(stream, profile, max_bitrate));
    compatible
        .into_iter()
        .chain(rest)
        .collect()
}

impl StreamService {
    pub fn new(cfg: StreamServiceConfig) -> Self {
        Self {
            ctx: cfg.ctx,
            item_id: cfg.item_id,
            requested_id: cfg.requested_id,
            show_ungrouped: cfg.show_ungrouped,
            stream_filter: cfg.stream_filter,
            user_id: cfg.user_id,
            group: None,
            stream: None,
            streams: vec![],
            device_profile: None,
            max_bitrate: None,
        }
    }

    /// Load the service from a pre-fetched media item (playbackinfo path).
    ///
    /// Populates `self.group`, `self.stream`, and `self.streams`. Must be called
    /// before any of the selection or ID-mapping methods.
    pub async fn load(&mut self, media: db::Media) -> anyhow::Result<()> {
        if media.kind == db::MediaKind::StreamGroup {
            if let Ok(Some(mut parent)) = db::Media::get_by_id(
                &self
                    .ctx
                    .db,
                &self.item_id,
            )
            .await
            {
                self.ctx
                    .addons
                    .refresh_streams(&mut parent, &self.ctx, self.user_id)
                    .await
                    .inspect_err(|e| tracing::error!("refresh_streams failed: {e:#}"));
            }
            self.resolve_stream_group(media)
                .await?;
            return Ok(());
        }

        let mut root = resolve_stream_root(
            &media,
            self.item_id,
            &self
                .ctx
                .db,
        )
        .await;

        self.ctx
            .addons
            .refresh_streams(&mut root, &self.ctx, self.user_id)
            .await
            .inspect_err(|e| tracing::error!("refresh_streams failed: {e:#}"));

        let root_kind = root
            .kind
            .clone();
        let db_streams = root
            .streams(
                &self
                    .ctx
                    .db,
            )
            .await?;
        let raw = if db_streams.is_empty() {
            // Root item can be the stream itself (e.g. locally-imported files)
            // but only when it carries a URL. Addon content uses the root as a
            // container — falling back to it when the addon returned no streams
            // would queue a probe against an item with no stream_info.
            if root
                .stream_info
                .is_some()
            {
                vec![root]
            } else {
                vec![]
            }
        } else {
            db_streams
        };

        let streams = db::StreamGroup::filter_sources(
            &self
                .ctx
                .db,
            raw,
            self.show_ungrouped,
        )
        .await;
        let streams = if let Some(sf) = self
            .stream_filter
            .as_ref()
            .filter(|sf| {
                !sf.rules
                    .is_empty()
            })
            .filter(|_| {
                matches!(root_kind, db::MediaKind::Movie | db::MediaKind::Episode)
            }) {
            let before = streams.len();
            let filtered = db::apply_stream_filter(sf, streams);
            debug!(
                streams_before = before,
                streams_after = filtered.len(),
                rules = sf
                    .rules
                    .len(),
                "stream filter applied"
            );
            filtered
        } else {
            debug!(
                has_filter = self
                    .stream_filter
                    .is_some(),
                "stream filter skipped"
            );
            streams
        };

        if streams.is_empty() {
            return Ok(());
        }
        self.stream = streams
            .first()
            .cloned();
        self.streams = streams;
        Ok(())
    }

    /// One-shot lookup for handlers that only need a single resolved stream (subtitles, video).
    ///
    /// Handles StreamGroup → best candidate, device preference, and explicit stream UUID.
    /// Returns the concrete `db::Media` to stream.
    pub async fn lookup(
        ctx: &AppContext,
        item_id: Uuid,
        requested_id: Option<Uuid>,
        device_key: Option<&str>,
        user_id: Option<Uuid>,
    ) -> anyhow::Result<db::Media> {
        let lookup_id = requested_id.unwrap_or(item_id);
        // Resolve the id the way PlaybackInfo does: `resolve_item` is
        // `get_by_id` plus the synthetic-id path (search/catalog ids that have
        // no row yet), so a client playing such an item reaches its streams.
        let media = crate::services::MediaResolveService::resolve_item(lookup_id, ctx)
            .await?
            .ok_or_else(|| anyhow::anyhow!("stream not found: {}", lookup_id))?;
        Self::dispatch_lookup(ctx, item_id, requested_id, device_key, user_id, media)
            .await
    }

    async fn dispatch_lookup(
        ctx: &AppContext,
        item_id: Uuid,
        requested_id: Option<Uuid>,
        device_key: Option<&str>,
        user_id: Option<Uuid>,
        media: db::Media,
    ) -> anyhow::Result<db::Media> {
        match media.kind {
            db::MediaKind::StreamGroup => {
                let gid = media.id;
                let mut candidates =
                    db::StreamGroup::streams_for(&ctx.db, &gid, &item_id).await?;
                if candidates.is_empty() {
                    return Err(anyhow::anyhow!(
                        "no streams available for group {}",
                        gid
                    ));
                }
                let cascade =
                    db::StreamGroup::streams_for_groups_after(&ctx.db, &gid, &item_id)
                        .await
                        .unwrap_or_default();
                candidates.extend(cascade);
                Ok(candidates.remove(0))
            }
            db::MediaKind::Movie | db::MediaKind::Episode | db::MediaKind::Track => {
                let mut media = media;
                let media_id = media.id;
                let _ = ctx
                    .addons
                    .refresh_streams(&mut media, ctx, user_id)
                    .await
                    .inspect_err(|e| tracing::error!("refresh_streams failed: {e:#}"));
                let sources = media
                    .streams(&ctx.db)
                    .await?;
                // Here `requested_id` resolved to a Movie/Episode/Track, so it
                // is an *item* id (auto-play, or the PlaybackInfo rewrite of
                // source[0].Id — the sibling item's UUID when duplicate items
                // share one IMDB id). An item's id is never one of its stream
                // ids, so matching it against `sources` can only fail; treat it
                // as auto-play and fall through to preference / first source.
                let specific_stream =
                    requested_id.filter(|&sid| sid != item_id && sid != media_id);
                if let Some(sid) = specific_stream {
                    sources
                        .into_iter()
                        .find(|s| s.id == sid)
                        .ok_or_else(|| anyhow::anyhow!("stream not found: {}", sid))
                } else if let Some(key) = device_key {
                    let saved = ctx
                        .store
                        .get::<Uuid>(&format!("pstream:{}:{}", item_id, key));
                    let by_pref = saved.and_then(|sid| {
                        sources
                            .iter()
                            .find(|s| s.id == *sid)
                            .cloned()
                    });
                    by_pref
                        .or_else(|| {
                            sources
                                .into_iter()
                                .next()
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!("no playable sources for {}", item_id)
                        })
                } else {
                    sources
                        .into_iter()
                        .next()
                        .ok_or_else(|| {
                            anyhow::anyhow!("no playable sources for {}", item_id)
                        })
                }
            }
            _ => Ok(media),
        }
    }

    async fn resolve_stream_group(&mut self, media: db::Media) -> anyhow::Result<()> {
        let gid = media.id;
        let gtitle = media
            .title
            .clone();
        let mut candidates = db::StreamGroup::streams_for(
            &self
                .ctx
                .db,
            &gid,
            &self.item_id,
        )
        .await?;
        if candidates.is_empty() {
            return Err(anyhow::anyhow!("no streams available for group {}", gid));
        }
        let cascade = db::StreamGroup::streams_for_groups_after(
            &self
                .ctx
                .db,
            &gid,
            &self.item_id,
        )
        .await
        .unwrap_or_default();
        candidates.extend(cascade);
        self.stream = Some(candidates[0].clone());
        self.group = Some((gid, gtitle, candidates));
        Ok(())
    }

    /// The concrete resolved stream. Panics if called before `resolve()`.
    pub fn candidate(&self) -> &db::Media {
        self.stream
            .as_ref()
            .expect("StreamService::load() must be called first")
    }

    /// The StreamGroup context, if the request was for a group.
    pub fn group(&self) -> Option<&(Uuid, String, Vec<db::Media>)> {
        self.group
            .as_ref()
    }

    /// UUID the client should see in `MediaSources[0].Id` and `TranscodingUrl MediaSourceId`.
    pub fn client_facing_id(&self) -> Uuid {
        self.group
            .as_ref()
            .map(|(gid, _, _)| *gid)
            .unwrap_or_else(|| {
                self.candidate()
                    .id
            })
    }

    /// UUID for `MediaSources[idx].Id`, using the probe-fallback effective stream.
    pub fn source_id_for(&self, effective: &db::Media) -> Uuid {
        self.group
            .as_ref()
            .map(|(gid, _, _)| *gid)
            .unwrap_or(effective.id)
    }

    /// Display name for `MediaSources[idx].Name`.
    pub fn source_name_for(&self, effective: &db::Media) -> String {
        self.group
            .as_ref()
            .map(|(_, t, _)| t.clone())
            .unwrap_or_else(|| {
                effective
                    .title
                    .clone()
            })
    }

    fn candidates(&self) -> &[db::Media] {
        self.group
            .as_ref()
            .map(|(_, _, c)| c.as_slice())
            .unwrap_or(&[])
    }

    /// Partition `self.streams` into candidate/probe lists and compute selection flags.
    pub(crate) fn select_streams(&self) -> StreamSelection {
        let all_streams = self
            .streams
            .clone();
        let item_id = self.item_id;
        let requested_id = self.requested_id;

        let specific_requested = self
            .group
            .is_some()
            || requested_id
                .map(|sid| {
                    sid != item_id
                        && all_streams
                            .iter()
                            .any(|s| s.id == sid)
                })
                .unwrap_or(false);

        if self
            .group
            .is_some()
        {
            return StreamSelection {
                candidates: vec![
                    self.candidate()
                        .clone(),
                ],
                probe_pool: self
                    .candidates()
                    .to_vec(),
                restrict_resolution: false,
                preferred_probe_id: None,
                specific_requested: true,
            };
        }

        let mut probe_pool = all_streams.clone();

        let (candidates, preferred_probe_id) = if specific_requested {
            let sid = requested_id.unwrap();
            (
                all_streams
                    .into_iter()
                    .filter(|s| s.id == sid)
                    .collect(),
                None,
            )
        } else if requested_id.is_some() {
            // media_source_id == item_id (Android TV auto-play) or stream not found:
            // return only the first stream; specific_requested stays false so
            // source[0].id is overridden to item_id below (required for Android TV routing).
            // When a device profile is known, serve device_aware_probe_pool's top
            // candidate instead of the raw addon-order first stream (#552); with no
            // profile (e.g. Android TV) this is unchanged from before.
            let v = if let Some(profile) = &self.device_profile {
                let mut pool = device_aware_probe_pool(
                    &all_streams,
                    Some(profile),
                    self.max_bitrate,
                );
                pool.truncate(1);
                pool
            } else {
                let mut v = all_streams;
                v.truncate(1);
                v
            };
            (v, None)
        } else {
            // No stream ID: return all versions for the selection UI,
            // but independently probe the strongest filename-derived candidate
            // first. This keeps addon order intact for Disabled mode while still
            // giving capability ranking the best available real probe. The
            // quality-ordered pool also preserves the previous fallback order.
            probe_pool = quality_ordered_probe_pool(&all_streams);
            let preferred = probe_pool
                .first()
                .map(|stream| stream.id);
            (all_streams, preferred)
        };

        StreamSelection {
            candidates,
            probe_pool,
            restrict_resolution: true,
            preferred_probe_id,
            specific_requested,
        }
    }

    /// Probe all stream candidates and return stamped results.
    ///
    /// Internally calls `select_streams()`, loads probe config, then invokes `probe_stream`
    /// for each candidate. Source ID/name/path/remux are stamped before returning so the
    /// handler only deals with playback-decision work.
    pub async fn probe_candidates(&self) -> anyhow::Result<ProbedStreams> {
        let sel = self.select_streams();
        let probe_cfg = db::Settings::get_config_or_default(
            &self
                .ctx
                .db,
        )
        .await;
        let timeout = probe_cfg
            .probe_timeout_secs
            .unwrap_or(20) as u64;
        let timeout_p2p = probe_cfg
            .probe_timeout_p2p_secs
            .unwrap_or(60) as u64;
        let auto_next = probe_cfg
            .auto_next_stream_on_probe_fail
            .unwrap_or(true);
        let max_retries = probe_cfg
            .max_probe_fallback_streams
            .unwrap_or(3) as usize;
        let port = self
            .ctx
            .config
            .port;
        let mut item = db::Media::get_by_id(
            &self
                .ctx
                .db,
            &self.item_id,
        )
        .await
        .ok()
        .flatten();
        if let Some(ref mut it) = item {
            it.grandparent(
                &self
                    .ctx
                    .db,
            )
            .await
            .ok();
        }

        let mut results = Vec::with_capacity(
            sel.candidates
                .len(),
        );
        for stream in sel
            .candidates
            .into_iter()
        {
            let url_opt = stream
                .stream_info
                .as_ref()
                .map(|si| {
                    si.descriptor
                        .server_input(stream.id, port)
                });
            let skip_probe = sel
                .preferred_probe_id
                .is_some_and(|preferred| stream.id != preferred)
                // A legacy/RemuxDB H.264 result without a usable ref-frame
                // count is deliberately stale. Probe it even when it is not
                // the preferred candidate so compatibility ranking has the
                // metadata it needs.
                && !stream
                    .probe_data
                    .as_ref()
                    .is_some_and(ProbeDataExt::needs_reprobe);
            // A filename guess is never a completed probe — it must not skip
            // submitting a freshly-probed result to RemuxDB.
            let was_cached = stream
                .probe_data
                .as_ref()
                .is_some_and(|pd| {
                    pd.video_stream()
                        .is_some()
                        && !pd.is_filename_guess()
                });
            let timeout_secs = if stream
                .stream_info
                .as_ref()
                .map_or(false, |si| si.is_p2p())
            {
                timeout_p2p
            } else {
                timeout
            };
            let (mut source, effective_stream) = probe_stream(
                &stream,
                url_opt,
                skip_probe,
                timeout_secs,
                auto_next,
                max_retries,
                &sel.probe_pool,
                sel.restrict_resolution,
                port,
                &self
                    .ctx
                    .db,
            )
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;

            // Use the StreamGroup UUID when this candidate is a group representative
            // (group_id is set by filter_sources). This ensures the client sends back
            // the stable group UUID, not a stream UUID that can change after a refresh.
            let (cid, name) = if let Some(gid) = stream.group_id {
                (
                    gid,
                    stream
                        .title
                        .clone(),
                )
            } else {
                (
                    self.source_id_for(&effective_stream),
                    self.source_name_for(&effective_stream),
                )
            };
            source.id = cid;
            source.e_tag = cid;
            source.name = Some(name);
            source.has_segments = true;
            // Include the release filename's stem when available (same
            // convention as MediaSourceInfo::from(db::Media) in
            // conversions.rs) so clients that surface `Path` as a display
            // field show the real release name instead of a bare UUID.
            // This path previously always dropped it, even when the addon
            // supplied behaviorHints.filename and it was sitting right
            // there in effective_stream.stream_info.
            let stem = effective_stream
                .stream_info
                .as_ref()
                .and_then(|si| {
                    si.filename
                        .as_deref()
                })
                .and_then(|f| {
                    std::path::Path::new(f)
                        .file_stem()
                        .and_then(|s| s.to_str())
                });
            source.path = Some(match stem {
                Some(s) => format!("/remux/{}/{}", effective_stream.id, s),
                None => format!("/remux/{}", effective_stream.id),
            });
            source.is_remote = false;
            // Re-apply binge-group headers — ffmpeg probing produces a fresh
            // MediaSourceInfo and would otherwise drop provider hints. Must
            // preserve whatever probe_source tag probe_stream() already set
            // (Ffprobe from a real probe, or carried over from cached data) —
            // a blanket ..Default::default() here would silently erase it.
            let probe_source = source
                .remux
                .as_ref()
                .and_then(|r| r.source);
            source.remux = Some(api::MediaSourceRemuxInfo {
                provider_info: effective_stream
                    .stream_info
                    .as_ref()
                    .and_then(|si| serde_json::to_value(si).ok()),
                source: probe_source,
            });

            let remuxdb_enabled = probe_cfg
                .remuxdb_enabled
                .unwrap_or(true);
            let is_remuxdb_kind = item
                .as_ref()
                .map_or(false, |it| {
                    matches!(it.kind, db::MediaKind::Movie | db::MediaKind::Episode)
                });
            if was_cached {
                debug!(id = %effective_stream.id, "remuxdb: skipping (probe cache hit)");
            } else if !remuxdb_enabled {
                debug!(id = %effective_stream.id, "remuxdb: skipping (disabled)");
            } else if !is_remuxdb_kind {
                debug!(id = %effective_stream.id, kind = ?item.as_ref().map(|it| &it.kind), "remuxdb: skipping (not movie/episode)");
            } else if source.is_filename_guess() {
                debug!(id = %effective_stream.id, "remuxdb: skipping (filename guess, not a real probe)");
            } else if let Some(url) = self
                .ctx
                .config
                .remuxdb_url
                .clone()
            {
                match media_info_from_probe(&source, &effective_stream, item.as_ref()) {
                    Some(mi) => {
                        debug!(id = %effective_stream.id, url, "remuxdb: submitting mediainfo");
                        let token = probe_cfg
                            .remuxdb_token
                            .clone();
                        tokio::spawn(mi.submit(url, token));
                    }
                    None => {
                        debug!(id = %effective_stream.id, "remuxdb: skipping (no stream_info or missing required fields)");
                    }
                }
            }

            results.push(ProbeResult {
                source,
                stream,
                effective_stream,
            });
        }

        Ok(ProbedStreams {
            results,
            specific_requested: sel.specific_requested,
        })
    }

    /// Remember that the probe fell over from the client-facing first source
    /// to `effective_stream` for this play session.
    ///
    /// PlaybackInfo stamps `MediaSources[0].Id` with the item id, so a client
    /// that auto-plays comes back to `/videos/{id}/stream` naming the item,
    /// not a stream. `dispatch_lookup` treats that as "first source" — the
    /// very stream that just failed to probe — and the fallback PlaybackInfo
    /// chose is lost: the stream request hangs on the dead source until the
    /// upstream timeout and fails, while the second source, picked by hand,
    /// plays at once. Keying on the play session id (minted by PlaybackInfo
    /// and echoed by every Jellyfin client on the stream URL) ties the two
    /// requests together without needing a device id, which not every client
    /// sends on stream URLs. A stream-group request answers with the group id
    /// the same way, so the record is keyed by the id the client echoes back:
    /// the item id, or the group id. No-op when nothing fell over or the
    /// client named a specific stream.
    pub fn save_probe_fallback(&self, play_session_id: &str, probed: &ProbedStreams) {
        let source_id = match &self.group {
            Some((gid, _, _)) => *gid,
            None if probed.specific_requested => return,
            None => self.item_id,
        };
        let Some(first) = probed
            .results
            .first()
        else {
            return;
        };
        if first
            .effective_stream
            .id
            == first
                .stream
                .id
        {
            return;
        }
        self.ctx
            .store
            .save(
                Self::probe_fallback_key(play_session_id, source_id),
                first
                    .effective_stream
                    .id,
                std::time::Duration::from_secs(24 * 3600),
            );
    }

    /// The stream PlaybackInfo's probe fell over to when it answered
    /// `play_session_id` with `source_id` (the item id or a group id), if any.
    pub fn probe_fallback_for(
        ctx: &AppContext,
        play_session_id: &str,
        source_id: Uuid,
    ) -> Option<Uuid> {
        ctx.store
            .get::<Uuid>(Self::probe_fallback_key(play_session_id, source_id))
            .map(|id| *id)
    }

    fn probe_fallback_key(play_session_id: &str, source_id: Uuid) -> String {
        format!("pstream:psid:{play_session_id}:{source_id}")
    }

    /// Persist the resolved stream UUID in the device-preference store (24 h TTL).
    /// Also records the group→item association so /Items/{group_uuid} can redirect
    /// to the correct content item without a DB scan.
    /// No-op when this was not a group request.
    pub fn save_preference(&self, device_key: &str) {
        let Some((gid, _, _)) = &self.group else {
            return;
        };
        self.ctx
            .store
            .save(
                format!("pstream:{}:{}", self.item_id, device_key),
                self.candidate()
                    .id,
                std::time::Duration::from_secs(24 * 3600),
            );
        if let Some(uid) = self.user_id {
            Self::save_group_item(
                &self
                    .ctx
                    .store,
                uid,
                *gid,
                self.item_id,
            );
        }
    }

    /// Record that `group_id` (a stream group UUID) belongs to `item_id` for the given user.
    ///
    /// Keyed per-user to avoid collisions when the same global group appears across multiple
    /// media items. Used by `/Items/{group_uuid}` to redirect back to the owning content item.
    /// TTL is 7 days — long enough to survive normal browsing sessions.
    pub fn save_group_item(
        store: &remux_utils::Store,
        user_id: Uuid,
        group_id: Uuid,
        item_id: Uuid,
    ) {
        store.save(
            format!("gitem:{}:{}", user_id, group_id),
            item_id,
            std::time::Duration::from_secs(7 * 24 * 3600),
        );
    }

    /// Look up the content item that owns `group_id` for `user_id`.
    ///
    /// Returns `None` when the user has not yet browsed an item that carries this stream group,
    /// or the mapping has expired. Callers should surface a 404 in that case.
    pub fn get_group_item(
        store: &remux_utils::Store,
        user_id: Uuid,
        group_id: Uuid,
    ) -> Option<Uuid> {
        store
            .get::<Uuid>(format!("gitem:{}:{}", user_id, group_id))
            .map(|id| *id)
    }
}

/// Result of `StreamService::select_streams` — partitioned candidate/probe lists and flags.
pub(crate) struct StreamSelection {
    /// Streams to present to the client and probe.
    pub candidates: Vec<db::Media>,
    /// Full pool used for probe-fallback across sibling streams.
    pub probe_pool: Vec<db::Media>,
    /// When false (group requests), cross-resolution fallback is intentional.
    pub restrict_resolution: bool,
    /// When present, this is the only candidate that receives a real probe;
    /// the others receive filename guesses without changing presentation order.
    pub preferred_probe_id: Option<Uuid>,
    /// True when the client named a specific stream — keep its UUID, don't override to item_id.
    pub specific_requested: bool,
}

fn media_info_from_probe(
    probe: &api::MediaSourceInfo,
    stream: &db::Media,
    item: Option<&db::Media>,
) -> Option<remuxdb::MediaInfoPayload> {
    let (info_hash, file_idx, nzb, filename) = match stream
        .stream_info
        .as_ref()
    {
        Some(si) => {
            let (hash, idx) = match si.torrent_identity() {
                Some((hash, idx)) => (Some(hash.to_owned()), idx),
                None => (None, None),
            };
            let nzb = si
                .usenet_guid
                .as_ref()
                .zip(
                    si.usenet_indexer
                        .as_ref(),
                )
                .map(|(guid, indexer)| remuxdb::NzbSubmission {
                    indexer: indexer.clone(),
                    indexer_guid: guid.clone(),
                    title: si
                        .filename
                        .clone(),
                });
            (
                hash,
                idx,
                nzb,
                si.filename
                    .clone()
                    .unwrap_or_else(|| {
                        stream
                            .title
                            .clone()
                    }),
            )
        }
        None => (
            None,
            None,
            None,
            stream
                .title
                .clone(),
        ),
    };

    if info_hash.is_none() && nzb.is_none() {
        return None;
    }

    let (kind, external_ids, season, episode) = if let Some(item) = item {
        let kind = match item.kind {
            db::MediaKind::Episode => "episode",
            _ => "movie",
        }
        .to_string();
        let imdb_id = item
            .external_ids
            .imdb
            .as_ref()
            .map(|v| v.to_string())
            .or_else(|| {
                item.grandparent
                    .as_deref()
                    .and_then(|gp| {
                        gp.external_ids
                            .imdb
                            .as_ref()
                    })
                    .map(|v| v.to_string())
            });
        let ids = (imdb_id.is_some()
            || item
                .external_ids
                .tmdb
                .is_some()
            || item
                .external_ids
                .tvdb
                .is_some()
            || item
                .external_ids
                .kitsu
                .is_some())
        .then(|| remuxdb::ExternalIds {
            imdb_id,
            tmdb_id: item
                .external_ids
                .tmdb,
            tvdb_id: item
                .external_ids
                .tvdb,
            kitsu_id: item
                .external_ids
                .kitsu,
        });
        let season = if item.kind == db::MediaKind::Episode {
            item.parent_idx
                .map(|v| v as i32)
        } else {
            None
        };
        let episode = if item.kind == db::MediaKind::Episode {
            item.idx
                .map(|v| v as i32)
        } else {
            None
        };
        (kind, ids, season, episode)
    } else {
        ("movie".to_string(), None, None, None)
    };

    let tracks = probe
        .media_streams
        .iter()
        .filter_map(|ms| remuxdb::TrackPayload::try_from(ms).ok())
        .collect();

    Some(remuxdb::MediaInfoPayload {
        client_id: Some(crate::common::server_id()),
        kind,
        filename,
        torrent_info_hash: info_hash,
        torrent_file_idx: file_idx,
        nzb,
        container: probe
            .container
            .as_ref()
            .map(|c| c.to_string())
            .unwrap_or_default(),
        size: probe
            .size
            .or_else(|| {
                stream
                    .stream_info
                    .as_ref()
                    .and_then(|si| si.size)
            })
            .filter(|&s| s > 0)?,
        duration: crate::common::ticks_to_seconds(
            probe
                .run_time_ticks
                .unwrap_or(0),
        ),
        bitrate: probe.bitrate,
        season,
        episode,
        external_ids,
        tracks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::{StreamDescriptor, StreamInfo};

    /// A `MediaSourceId` that is an item id (auto-play, or the PlaybackInfo
    /// rewrite of `source[0].Id` — the sibling's UUID when duplicate items share
    /// one IMDB id) must resolve to a stream. Before the fix the Movie arm looked
    /// for the item's own id among its streams, failed with "stream not found",
    /// and the stream endpoint served the no-streams placeholder (#212): source
    /// #1 of every affected title played blank while the rest worked.
    #[tokio::test]
    async fn lookup_treats_item_id_as_auto_play_not_stream_id() {
        use crate::integration_test::{
            authenticated_server, insert_test_source, seed_movie,
        };

        let (_server, guard, _token) = authenticated_server().await;
        let ctx = &guard.0;

        // Movie A owns one stream. `Media::save` is an upsert that doesn't
        // update `parent_id`, so attach the row directly. Stamp
        // `streams_refreshed_at` 30s back: `refresh_streams` (no addons here)
        // takes its TTL fast path, and `Media::streams()` keeps the row.
        let owner = seed_movie(ctx).await;
        let stream = insert_test_source(ctx).await;
        sqlx::query("UPDATE media SET parent_id = ? WHERE id = ?")
            .bind(owner.id)
            .bind(stream.id)
            .execute(&ctx.db)
            .await
            .unwrap();
        sqlx::query("UPDATE media SET streams_refreshed_at = ? WHERE id = ?")
            .bind(chrono::Utc::now().naive_utc() - chrono::Duration::seconds(30))
            .bind(owner.id)
            .execute(&ctx.db)
            .await
            .unwrap();

        // Movie B: an unrelated item with no stream rows of its own. Sharing
        // `owner`'s external ids used to be how this scenario arose (two
        // rows for the same film) — that's now prevented at the DB level,
        // but the code path under test only cares that B's id is a real,
        // distinct Movie row mistakenly handed back as a MediaSourceId, not
        // that it represents the same content as A, so it just needs its
        // own (any) external id to satisfy validation.
        let mut dup = db::Media {
            id: Uuid::new_v4(),
            title: owner
                .title
                .clone(),
            kind: db::MediaKind::Movie,
            external_ids: db::ExternalIds {
                imdb: db::NonEmptyString::try_new("tt9999998").ok(),
                ..Default::default()
            },
            ..Default::default()
        };
        dup.save(&ctx.db)
            .await
            .unwrap();

        // B played with MediaSourceId = A.id (what the auto-play rewrite hands
        // out). Before the fix: Err("stream not found: <A.id>").
        let resolved = StreamService::lookup(ctx, dup.id, Some(owner.id), None, None)
            .await
            .expect("an item id used as MediaSourceId must resolve to a stream");
        assert_eq!(resolved.id, stream.id);

        // Plain auto-play (MediaSourceId == item being played) still works.
        let resolved = StreamService::lookup(ctx, owner.id, Some(owner.id), None, None)
            .await
            .unwrap();
        assert_eq!(resolved.id, stream.id);

        // A real stream id is still honoured.
        let resolved =
            StreamService::lookup(ctx, owner.id, Some(stream.id), None, None)
                .await
                .unwrap();
        assert_eq!(resolved.id, stream.id);
    }

    const DEBRID_HASH: &str = "63259f55cd5c31826321286ae1fde40c931dee1d";
    const DESCRIPTOR_HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// Minimal successful probe: only `size` is required by `media_info_from_probe`.
    fn probe_with_size(size: i64) -> api::MediaSourceInfo {
        api::MediaSourceInfo {
            size: Some(size),
            ..Default::default()
        }
    }

    fn stream_media(info: StreamInfo) -> db::Media {
        db::Media {
            title: "Example".to_string(),
            stream_info: Some(info),
            ..Default::default()
        }
    }

    fn quality_stream(filename: &str) -> db::Media {
        let mut stream = stream_media(StreamInfo {
            descriptor: StreamDescriptor::http(format!(
                "https://example.test/{filename}"
            )),
            filename: Some(filename.to_string()),
            ..Default::default()
        });
        stream.id = Uuid::new_v4();
        stream
    }

    #[test]
    fn quality_probe_order_does_not_mutate_addon_order() {
        let original = vec![
            quality_stream("Movie.2026.720p.WEBRip.mkv"),
            quality_stream("Movie.2026.2160p.BluRay.Remux.mkv"),
            quality_stream("Movie.2026.1080p.WEB-DL.mkv"),
        ];
        let original_ids: Vec<_> = original
            .iter()
            .map(|stream| stream.id)
            .collect();

        let probe_pool = quality_ordered_probe_pool(&original);
        let candidate_ids: Vec<_> = original
            .iter()
            .map(|stream| stream.id)
            .collect();
        let probe_ids: Vec<_> = probe_pool
            .iter()
            .map(|stream| stream.id)
            .collect();

        assert_eq!(candidate_ids, original_ids);
        assert_eq!(
            probe_ids,
            vec![original_ids[1], original_ids[2], original_ids[0]]
        );
    }

    #[test]
    fn media_info_from_probe_preserves_torrent_identity_for_http_debrid_stream() {
        let stream = stream_media(StreamInfo {
            descriptor: StreamDescriptor::http("https://debrid.example/playback/123"),
            torrent_info_hash: Some(DEBRID_HASH.to_string()),
            torrent_file_idx: Some(4),
            filename: Some("Example.2026.2160p.mkv".to_string()),
            ..Default::default()
        });
        let payload = media_info_from_probe(
            &probe_with_size(12_340_295_313),
            &stream,
            None,
        )
        .expect("HTTP debrid stream with preserved torrent identity must be submitted");
        assert_eq!(
            payload
                .torrent_info_hash
                .as_deref(),
            Some(DEBRID_HASH)
        );
        assert_eq!(payload.torrent_file_idx, Some(4));
        assert!(
            payload
                .nzb
                .is_none()
        );
    }

    #[test]
    fn media_info_from_probe_prefers_torrent_descriptor_identity() {
        let stream = stream_media(StreamInfo {
            descriptor: StreamDescriptor::Torrent {
                info_hash: DESCRIPTOR_HASH.to_string(),
                file_hint: None,
                file_idx: Some(7),
                trackers: vec![],
            },
            torrent_info_hash: Some(DEBRID_HASH.to_string()),
            torrent_file_idx: Some(4),
            ..Default::default()
        });
        let payload =
            media_info_from_probe(&probe_with_size(1), &stream, None).unwrap();
        assert_eq!(
            payload
                .torrent_info_hash
                .as_deref(),
            Some(DESCRIPTOR_HASH)
        );
        assert_eq!(payload.torrent_file_idx, Some(7));
    }

    #[test]
    fn media_info_from_probe_http_debrid_without_file_idx_still_submits() {
        let stream = stream_media(StreamInfo {
            descriptor: StreamDescriptor::http("https://debrid.example/playback/123"),
            torrent_info_hash: Some(DEBRID_HASH.to_string()),
            torrent_file_idx: None,
            ..Default::default()
        });
        let payload =
            media_info_from_probe(&probe_with_size(1), &stream, None).unwrap();
        assert_eq!(
            payload
                .torrent_info_hash
                .as_deref(),
            Some(DEBRID_HASH)
        );
        assert_eq!(payload.torrent_file_idx, None);
    }

    #[test]
    fn media_info_from_probe_nzb_only_stream_submits_without_torrent_identity() {
        let stream = stream_media(StreamInfo {
            descriptor: StreamDescriptor::http("https://usenet.example/file"),
            usenet_guid: Some("guid-123".to_string()),
            usenet_indexer: Some("NZBgeek".to_string()),
            filename: Some("Example.2026.1080p.mkv".to_string()),
            ..Default::default()
        });
        let payload = media_info_from_probe(&probe_with_size(1), &stream, None)
            .expect("usenet stream must still be submitted");
        let nzb = payload
            .nzb
            .expect("nzb submission populated");
        assert_eq!(nzb.indexer, "NZBgeek");
        assert_eq!(nzb.indexer_guid, "guid-123");
        assert_eq!(
            nzb.title
                .as_deref(),
            Some("Example.2026.1080p.mkv")
        );
        assert!(
            payload
                .torrent_info_hash
                .is_none()
        );
        assert!(
            payload
                .torrent_file_idx
                .is_none()
        );
    }

    #[test]
    fn media_info_from_probe_skips_http_stream_without_any_identity() {
        let stream = stream_media(StreamInfo {
            descriptor: StreamDescriptor::http("https://cdn.example/file.mkv"),
            ..Default::default()
        });
        assert!(media_info_from_probe(&probe_with_size(1), &stream, None).is_none());
    }

    /// A probe fallback must reach the stream request that follows PlaybackInfo.
    /// That request names the item, not a stream, so without this the resolver
    /// serves the first source — the one that just failed — and playback hangs
    /// until the upstream timeout while the second source, picked by hand, plays.
    #[tokio::test]
    async fn probe_fallback_is_remembered_per_play_session() {
        use crate::integration_test::{
            authenticated_server, insert_test_source, seed_movie,
        };
        let (_server, guard, _token) = authenticated_server().await;
        let ctx = &guard.0;
        let owner = seed_movie(ctx).await;
        let dead = insert_test_source(ctx).await;
        let alive = insert_test_source(ctx).await;
        assert_ne!(dead.id, alive.id);
        let service = StreamService::new(StreamServiceConfig {
            ctx: ctx.clone(),
            item_id: owner.id,
            requested_id: None,
            show_ungrouped: true,
            stream_filter: None,
            user_id: None,
        });
        let probed = |effective: &db::Media, specific_requested: bool| ProbedStreams {
            results: vec![ProbeResult {
                source: api::MediaSourceInfo::from(dead.clone()),
                stream: dead.clone(),
                effective_stream: effective.clone(),
            }],
            specific_requested,
        };

        // Fell over to `alive`: remembered under the play session.
        service.save_probe_fallback("psid-fallback", &probed(&alive, false));
        assert_eq!(
            StreamService::probe_fallback_for(ctx, "psid-fallback", owner.id),
            Some(alive.id)
        );
        // First source probed fine: nothing to remember.
        service.save_probe_fallback("psid-clean", &probed(&dead, false));
        assert_eq!(
            StreamService::probe_fallback_for(ctx, "psid-clean", owner.id),
            None
        );
        // Client named a specific stream: its choice stands, nothing remembered.
        service.save_probe_fallback("psid-specific", &probed(&alive, true));
        assert_eq!(
            StreamService::probe_fallback_for(ctx, "psid-specific", owner.id),
            None
        );
        // Unknown session: nothing.
        assert_eq!(
            StreamService::probe_fallback_for(ctx, "psid-unknown", owner.id),
            None
        );
    }

    /// With stream groups on, the initial PlaybackInfo lists one representative
    /// stream per group and is not a specific request, so a fallback is
    /// remembered under the item id exactly as without groups. A request for a
    /// group by its UUID answers with the group id, so its fallback is
    /// remembered under the group id.
    #[tokio::test]
    async fn probe_fallback_with_stream_groups() {
        use crate::integration_test::{
            authenticated_server, insert_test_source, seed_movie,
        };
        let (_server, guard, _token) = authenticated_server().await;
        let ctx = &guard.0;
        let owner = seed_movie(ctx).await;
        let group_a = uuid::Uuid::new_v4();
        let group_b = uuid::Uuid::new_v4();
        let mut dead = insert_test_source(ctx).await;
        let mut alive = insert_test_source(ctx).await;
        dead.group_id = Some(group_a);
        alive.group_id = Some(group_b);
        let probed = |specific_requested: bool| ProbedStreams {
            results: vec![ProbeResult {
                source: api::MediaSourceInfo::from(dead.clone()),
                stream: dead.clone(),
                effective_stream: alive.clone(),
            }],
            specific_requested,
        };

        // Initial load: group representatives, no group context.
        let mut service = StreamService::new(StreamServiceConfig {
            ctx: ctx.clone(),
            item_id: owner.id,
            requested_id: None,
            show_ungrouped: false,
            stream_filter: None,
            user_id: None,
        });
        service.streams = vec![dead.clone(), alive.clone()];
        let selection = service.select_streams();
        assert!(!selection.specific_requested);
        service
            .save_probe_fallback("psid-grouped", &probed(selection.specific_requested));
        assert_eq!(
            StreamService::probe_fallback_for(ctx, "psid-grouped", owner.id),
            Some(alive.id)
        );

        // Group A requested by its UUID.
        let mut service = StreamService::new(StreamServiceConfig {
            ctx: ctx.clone(),
            item_id: owner.id,
            requested_id: Some(group_a),
            show_ungrouped: false,
            stream_filter: None,
            user_id: None,
        });
        service.group = Some((
            group_a,
            "Group A".to_string(),
            vec![dead.clone(), alive.clone()],
        ));
        service.stream = Some(dead.clone());
        service.streams = vec![dead.clone(), alive.clone()];
        let selection = service.select_streams();
        assert!(selection.specific_requested);
        service.save_probe_fallback(
            "psid-group-request",
            &probed(selection.specific_requested),
        );
        assert_eq!(
            StreamService::probe_fallback_for(ctx, "psid-group-request", group_a),
            Some(alive.id)
        );
    }

    // =========================================================================
    // Reproduction: Chrome web client offered a 4K DoVi HEVC remux instead of
    // a 1080p H.264 file under `SortMediaSourcesMode::Compatibility`. See
    // `device_profile.rs`'s `repro_*` tests for the post-probe capability-sort
    // half of this investigation. These two tests cover the pre-probe layer:
    // which candidate becomes the probe target / the auto-play pick, before
    // any device profile is even consulted.

    /// Real filenames for the 19-version scenario (same set as
    /// `device_profile.rs`'s `repro_candidates()`, filenames only — this
    /// layer only cares about `quality_weight()`, which is filename-derived).
    /// `A` (the 2160p REMUX actually served to Chrome in production) is
    /// deliberately placed in the *middle* of the list, not first, to prove
    /// the pre-probe pick is driven by parsed quality, not by addon/DB order.
    fn repro_production_filenames() -> Vec<&'static str> {
        vec![
            "Toy.Story.5.2025.1080p.BluRay.x264.AAC-GROUP0.mkv",
            "Toy.Story.5.2025.2160p.WEB-DL.HDR10.HEVC.DDP5.1-GROUP3.mkv",
            "Toy.Story.5.2025.1080p.WEB-DL.AAC.H.264-GROUP1.mkv",
            "Toy.Story.5.2025.2160p.UHD.BluRay.REMUX.DV.HDR.HEVC.TrueHD.Atmos.7.1-FraMeSToR.mkv", // A
            "Toy.Story.5.2025.2160p.WEB-DL.DV.HDR.HEVC.DDP5.1-GROUP0.mkv",
            "Toy.Story.5.2025.1080p.WEB-DL.DDP5.1.H.264-GROUP3.mkv",
            "Toy.Story.5.2025.2160p.WEB-DL.HDR10.HEVC.TrueHD.Atmos.7.1-GROUP1.mkv",
            "Toy.Story.5.2025.1080p.BluRay.x264.DDP5.1-GROUP4.mkv",
        ]
    }

    /// `quality_ordered_probe_pool` takes no `DeviceProfile` argument at all
    /// (see its doc comment: "used only to choose probe attempt order before
    /// real technical specs are available" / "must not be used to mutate the
    /// persisted addon source order") — it ranks purely by
    /// `PreProbeQualityExt::quality_weight()`, i.e. filename-derived
    /// resolution + release-source tier. A 2160p BluRay REMUX scores the
    /// maximum possible weight `(5, 6)`, strictly higher than every other
    /// 2160p (WEB-DL/BluRay-encode, weight `(5, <6)`) or 1080p (`(4, _)`)
    /// candidate, so it is the pre-probe pick for every client — Chrome
    /// included — regardless of whether Chrome can actually play DoVi/TrueHD.
    #[test]
    fn repro_chrome_prefers_4k_remux_pre_probe() {
        let filenames = repro_production_filenames();
        let streams: Vec<db::Media> = filenames
            .iter()
            .map(|f| quality_stream(f))
            .collect();

        let pool = quality_ordered_probe_pool(&streams);
        let preferred = pool
            .first()
            .expect("pool must not be empty");
        let preferred_filename = preferred
            .stream_info
            .as_ref()
            .and_then(|si| {
                si.filename
                    .as_deref()
            })
            .unwrap_or_default();

        println!("pre-probe preferred candidate (device-blind): {preferred_filename}");

        assert!(
            preferred_filename.contains("REMUX"),
            "quality_ordered_probe_pool has no DeviceProfile parameter — it must \
             pick the highest filename-derived quality_weight() candidate \
             regardless of which client is asking. Got {preferred_filename}"
        );
        // TODO(patch): desired = the pre-probe pick should only decide probe
        // ORDER (which candidate gets a fresh ffprobe first), never which
        // candidate is actually served for playback — that decision belongs
        // entirely to the post-probe, device-aware capability sort.
    }

    /// `select_streams()`'s "requested_id == item_id" branch — the auto-play
    /// signal real clients send (Android TV, and Chrome/jellyfin-web's
    /// initial `MediaSourceId` on some playback paths) per the existing
    /// comment at its call site: "media_source_id == item_id (Android TV
    /// auto-play) ... return only the first stream; specific_requested stays
    /// false". This truncates `self.streams` to its first element *before*
    /// `quality_ordered_probe_pool` or any device-capability ranking ever
    /// runs. If the addon/DB happens to list the flashiest 4K release first
    /// (a common addon behaviour — biggest/most "definitive" release first),
    /// auto-play serves exactly that release, on every device, with no
    /// ranking of any kind involved.
    #[tokio::test]
    async fn repro_auto_play_media_source_id_bypasses_all_ranking() {
        use crate::integration_test::authenticated_server;

        let (_server, guard, _token) = authenticated_server().await;
        let ctx = &guard.0;
        let item_id = Uuid::new_v4();

        // Addon/DB order: candidate A (the 2160p REMUX) happens to be first —
        // exactly what an addon that lists biggest/"best" release first would
        // return, and unrelated to any device capability.
        let mut filenames = repro_production_filenames();
        filenames.retain(|f| !f.contains("REMUX"));
        filenames.insert(
            0,
            "Toy.Story.5.2025.2160p.UHD.BluRay.REMUX.DV.HDR.HEVC.TrueHD.Atmos.7.1-FraMeSToR.mkv",
        );
        let streams: Vec<db::Media> = filenames
            .iter()
            .map(|f| quality_stream(f))
            .collect();

        let mut service = StreamService::new(StreamServiceConfig {
            ctx: ctx.clone(),
            item_id,
            // The auto-play signal: MediaSourceId == the item being played.
            requested_id: Some(item_id),
            show_ungrouped: false,
            stream_filter: None,
            user_id: None,
        });
        service.streams = streams.clone();

        let sel = service.select_streams();

        assert_eq!(
            sel.candidates
                .len(),
            1,
            "auto-play truncates to a single candidate before any \
             device-capability ranking (or even quality_ordered_probe_pool) runs"
        );
        let picked = &sel.candidates[0];
        let picked_filename = picked
            .stream_info
            .as_ref()
            .and_then(|si| {
                si.filename
                    .as_deref()
            })
            .unwrap_or_default();
        println!("auto-play (MediaSourceId == item_id) picked: {picked_filename}");

        assert_eq!(
            picked.id, streams[0].id,
            "select_streams() returns literally whichever stream is first in \
             self.streams for auto-play — not the quality-ordered pick, and \
             not a capability-ranked pick. Picked {picked_filename}"
        );
        // TODO(patch): desired = the auto-play path should route through the
        // same `quality_ordered_probe_pool` + post-probe capability-sort
        // pipeline as an explicit PlaybackInfo request, instead of trusting
        // positional order in `self.streams`.
    }

    // =========================================================================
    // Desired behaviour (lostb1t/remux#552): pre-probe / auto-play selection
    // becomes device-aware. See `device_profile.rs`'s `repro_*`/`desired_*`
    // tests for the post-probe capability-sort half of this fix.
    //
    // These fixtures build `db::Media` directly (not `MediaSourceInfo`), in
    // the same probe_data-populated / filename-guess-only shape as
    // `device_profile.rs`'s `repro_probed_candidate`/`repro_guessed_candidate`,
    // because `device_aware_probe_pool` (unlike the post-probe capability
    // sort) operates pre-probe on `db::Media`.
    //
    // Candidates `A` and `B` are kept above the 65,573,770 bps (~65.6 Mbps)
    // cap used below, so bitrate (rule c) alone disqualifies them regardless
    // of profile. Candidate `D` is deliberately kept UNDER that cap so rule
    // (b) (VideoRangeType) can be exercised on its own: the repo
    // jellyfin-web fixture's hevc `CodecProfile` allows
    // SDR/HDR10/HDR10Plus/HLG/DOVI, so `D` is genuinely compatible there
    // (and correctly outranks 1080p — HDR is not gated, per lead review);
    // the live-like profile (`jellyfin_web_live_profile`, see below) narrows
    // that list to exclude DOVI, so `D` must fail rule (b) there instead.
    // Container is explicitly not considered by rule (a), and the hevc/h264
    // codecs themselves are allowed by jellyfin-web's mp4/m4v
    // DirectPlayProfile regardless of profile variant.

    /// Same jellyfin-web 10.11 profile as `device_profile.rs`'s
    /// `jellyfin_web_real_profile()`. Duplicated here (test-only) because
    /// that helper lives in a different module's private `#[cfg(test)]`
    /// block.
    fn jellyfin_web_real_profile() -> api::DeviceProfile {
        serde_json::from_str(include_str!(
            "../testdata/jellyfin_web_device_profile.json"
        ))
        .expect("fixture must parse")
    }

    /// Same jellyfin-web profile as the repo fixture, but with the HEVC
    /// `CodecProfile`'s `VideoRangeType` condition narrowed to
    /// `SDR|HDR10|HDR10Plus|HLG` (no `DOVI`) and every video
    /// `DirectPlayProfile`'s `AudioCodec` narrowed to
    /// `aac,mp3,mp2,opus,flac,vorbis` (no `ac3`/`eac3`) — this is what the
    /// user's actual Chrome instance sends (verified against the instance DB
    /// and the live `VideoRangeTypeNotSupported` reason observed on a DoVi
    /// title). Duplicated from `device_profile.rs`'s helper of the same
    /// intent for the same cross-module-private-test-mod reason as above.
    fn jellyfin_web_live_profile() -> api::DeviceProfile {
        let mut value: serde_json::Value = serde_json::from_str(include_str!(
            "../testdata/jellyfin_web_device_profile.json"
        ))
        .expect("fixture must parse as JSON");

        if let Some(profiles) = value
            .get_mut("DirectPlayProfiles")
            .and_then(|v| v.as_array_mut())
        {
            for profile in profiles {
                if profile
                    .get("Type")
                    .and_then(|t| t.as_str())
                    == Some("Video")
                {
                    profile["AudioCodec"] = serde_json::Value::String(
                        "aac,mp3,mp2,opus,flac,vorbis".to_string(),
                    );
                }
            }
        }

        if let Some(codec_profiles) = value
            .get_mut("CodecProfiles")
            .and_then(|v| v.as_array_mut())
        {
            for cp in codec_profiles {
                let is_hevc = cp
                    .get("Codec")
                    .and_then(|c| c.as_str())
                    == Some("hevc");
                if !is_hevc {
                    continue;
                }
                if let Some(conditions) = cp
                    .get_mut("Conditions")
                    .and_then(|v| v.as_array_mut())
                {
                    for cond in conditions {
                        if cond
                            .get("Property")
                            .and_then(|p| p.as_str())
                            == Some("VideoRangeType")
                        {
                            cond["Value"] = serde_json::Value::String(
                                "SDR|HDR10|HDR10Plus|HLG".to_string(),
                            );
                        }
                    }
                }
            }
        }

        serde_json::from_value(value)
            .expect("mutated fixture must still parse as a DeviceProfile")
    }

    /// True when `reasons` contains none of the reasons that force a real
    /// video re-encode (as opposed to a cheap audio/container remux).
    fn has_no_video_reencode_reason(reasons: &api::TranscodeReasons) -> bool {
        !reasons
            .0
            .iter()
            .any(|r| {
                matches!(
                    r,
                    api::TranscodeReason::VideoCodecNotSupported(_)
                        | api::TranscodeReason::VideoRangeTypeNotSupported(_)
                        | api::TranscodeReason::VideoLevelNotSupported(_)
                        | api::TranscodeReason::VideoProfileNotSupported(_)
                        | api::TranscodeReason::ContainerBitrateExceedsLimit
                )
            })
    }

    /// Same Infuse-like DoVi/TrueHD-capable profile as `device_profile.rs`'s
    /// `infuse_like_dovi_profile()`. Duplicated here for the same reason.
    fn infuse_like_dovi_profile() -> api::DeviceProfile {
        api::DeviceProfile {
            max_streaming_bitrate: Some(200_000_000),
            direct_play_profiles: vec![api::DirectPlayProfile {
                container: Some(vec![
                    api::VideoContainer::Mkv,
                    api::VideoContainer::Mp4,
                ]),
                video_codec: Some(vec![
                    api::VideoCodec::Hevc,
                    api::VideoCodec::H264,
                    api::VideoCodec::Av1,
                ]),
                audio_codec: Some(vec![
                    api::AudioCodec::TrueHd,
                    api::AudioCodec::Eac3,
                    api::AudioCodec::Ac3,
                    api::AudioCodec::Aac,
                    api::AudioCodec::Dts,
                    api::AudioCodec::Flac,
                ]),
                type_: Some(api::DlnaProfileType::Video),
            }],
            codec_profiles: vec![api::CodecProfile {
                type_: Some(api::CodecProfileType::Video),
                codec: Some(vec!["hevc".to_string()]),
                conditions: vec![api::ProfileCondition {
                    condition: Some(api::ProfileConditionType::EqualsAny),
                    property: Some(api::ProfileConditionProperty::VideoRangeType),
                    value: Some(
                        "SDR|HDR10|HDR10Plus|HLG|DOVI|DOVIWithHDR10".to_string(),
                    ),
                    is_required: Some(false),
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// A candidate with `probe_data` already populated (mirrors
    /// `source=remux_db`), built the same way
    /// `device_profile.rs::repro_probed_candidate` builds its underlying
    /// `db::Media`, minus the `MediaSourceInfo` conversion step —
    /// `device_aware_probe_pool` reads `db::Media.probe_data` directly.
    fn device_aware_probed_media(
        filename: &str,
        width: i64,
        height: i64,
        video_codec: &str,
        video_range: Option<VideoRangeType>,
        audio_codec: &str,
        size_gb: f64,
        bitrate_mbps: f64,
    ) -> db::Media {
        let size = (size_gb * 1_000_000_000.0) as i64;
        let bitrate = (bitrate_mbps * 1_000_000.0) as i64;
        let mut media = stream_media(StreamInfo {
            filename: Some(filename.to_string()),
            size: Some(size),
            ..Default::default()
        });
        media.id = Uuid::new_v4();
        media.title = "Toy Story 5".to_string();
        media.probe_data = Some(api::MediaSourceInfo {
            media_streams: vec![
                api::MediaStream {
                    type_: Some(MediaStreamType::Video),
                    codec: Some(video_codec.to_string()),
                    width: Some(width),
                    height: Some(height),
                    video_range_type: video_range,
                    ..Default::default()
                },
                api::MediaStream {
                    type_: Some(MediaStreamType::Audio),
                    codec: Some(audio_codec.to_string()),
                    ..Default::default()
                },
            ],
            container: Some(api::VideoContainer::Mkv),
            bitrate: Some(bitrate),
            size: Some(size),
            ..Default::default()
        });
        media
    }

    /// A candidate with no stored probe data at all (mirrors
    /// `source=filename_guess`) — `device_aware_probe_pool` must fall back to
    /// a filename guess for its video codec/range/bitrate, same as
    /// `device_profile.rs::repro_guessed_candidate`'s underlying `db::Media`.
    fn device_aware_guessed_media(
        filename: &str,
        size_gb: f64,
        runtime_secs: i64,
    ) -> db::Media {
        let size = (size_gb * 1_000_000_000.0) as i64;
        let mut media = stream_media(StreamInfo {
            filename: Some(filename.to_string()),
            size: Some(size),
            ..Default::default()
        });
        media.id = Uuid::new_v4();
        media.title = "Toy Story 5".to_string();
        media.runtime = Some(runtime_secs);
        media
    }

    /// A small Toy Story 5 scenario in the same spirit as
    /// `device_profile.rs::repro_candidates()`'s 19-candidate scenario: one
    /// 2160p DoVi HEVC REMUX (`A`), a second 2160p HEVC HDR10 alternate
    /// (`B`), two probed 1080p H.264 candidates (`C1`/`C2`), one
    /// filename-guess-only 1080p H.264 candidate (`C3`) exercising the
    /// "else filename guess" half of the spec's compatibility rule (a), and
    /// a correctly-tagged 2160p DOVI HEVC candidate (`D`) UNDER the bitrate
    /// cap, exercising rule (b) (VideoRangeType) on its own.
    fn device_aware_candidates() -> Vec<db::Media> {
        const RUNTIME_SECS: i64 = 8280;
        vec![
            // A: the 2160p DoVi HEVC REMUX actually served in production.
            device_aware_probed_media(
                "Toy.Story.5.2025.2160p.UHD.BluRay.REMUX.DV.HDR.HEVC.TrueHD.Atmos.7.1-FraMeSToR.mkv",
                3840,
                2160,
                "hevc",
                Some(VideoRangeType::Dovi),
                "truehd",
                52.4,
                68.6,
            ),
            // B: a second 2160p HEVC alternate (HDR10). Bitrate deliberately
            // kept above the 65.6 Mbps cap used below (see module doc
            // comment): jellyfin-web's hevc VideoRangeType condition allows
            // HDR10, so bitrate is the only thing that can disqualify it
            // under the spec's simplified compatibility rule.
            device_aware_probed_media(
                "Toy.Story.5.2025.2160p.WEB-DL.HDR10.HEVC.DDP5.1-GROUP0.mkv",
                3840,
                2160,
                "hevc",
                Some(VideoRangeType::Hdr10),
                "eac3",
                24.0,
                78.0,
            ),
            // C1/C2: compatible 1080p H.264 candidates, well under the cap.
            device_aware_probed_media(
                "Toy.Story.5.2025.1080p.BluRay.x264.AAC-GROUP0.mkv",
                1920,
                1080,
                "h264",
                Some(VideoRangeType::Sdr),
                "aac",
                2.3,
                8.0,
            ),
            device_aware_probed_media(
                "Toy.Story.5.2025.1080p.WEB-DL.DDP5.1.H.264-GROUP3.mkv",
                1920,
                1080,
                "h264",
                Some(VideoRangeType::Sdr),
                "eac3",
                6.7,
                18.5,
            ),
            // C3: filename-guess-only 1080p candidate (no probe_data).
            device_aware_guessed_media(
                "Toy.Story.5.2025.1080p.WEB-DL.AAC.H.264-GROUP1.mkv",
                3.9,
                RUNTIME_SECS,
            ),
            // D: a correctly-tagged 2160p DOVI HEVC candidate UNDER the 65.6
            // Mbps cap — bitrate alone (rule c) cannot disqualify it, so
            // this exercises rule (b) (VideoRangeType) on its own. Under a
            // profile whose hevc CodecProfile still allows DOVI (the repo
            // fixture), D is genuinely compatible and correctly outranks
            // 1080p by resolution — that is desired, device-aware behaviour.
            // Under a profile that does not allow DOVI (the live-like
            // profile), D must fail rule (b) and rank after every
            // compatible candidate.
            device_aware_probed_media(
                "Toy.Story.5.2025.2160p.WEB-DL.DV.HDR.HEVC.DDP5.1-DOVIUNDERCAP.mkv",
                3840,
                2160,
                "hevc",
                Some(VideoRangeType::Dovi),
                "eac3",
                20.0,
                28.0,
            ),
        ]
    }

    /// SPEC: with `profile == None`, `device_aware_probe_pool` returns exactly `quality_ordered_probe_pool(streams)` — byte-for-byte the same order.
    #[test]
    fn desired_device_aware_probe_pool_with_no_profile_matches_quality_ordered_pool() {
        let streams = device_aware_candidates();

        let expected: Vec<Uuid> = quality_ordered_probe_pool(&streams)
            .iter()
            .map(|s| s.id)
            .collect();
        let actual: Vec<Uuid> = device_aware_probe_pool(&streams, None, None)
            .iter()
            .map(|s| s.id)
            .collect();

        assert_eq!(
            actual, expected,
            "device_aware_probe_pool(streams, None, _) must be byte-for-byte \
             identical to quality_ordered_probe_pool(streams)"
        );
    }

    /// Runs `device_aware_probe_pool` against `profile` at `max_bitrate` and
    /// asserts the "honest, profile-agnostic" outcome per lead review: the
    /// winner must not need a video re-encode (checked against the same
    /// profile/cap via the full `compute_transcode_reasons` engine — a
    /// stronger, more meaningful check than device_aware_probe_pool's own
    /// simplified rules), and the 52.4 GB / 68.6 Mbps remux (candidate `A`)
    /// must never win at this cap. A compatible, in-cap HDR/DoVi 2160p
    /// candidate legitimately winning (e.g. candidate `D`) is NOT an error.
    fn assert_device_aware_pool_outcome(
        profile: &api::DeviceProfile,
        max_bitrate: u64,
        label: &str,
    ) {
        let streams = device_aware_candidates();
        let pool = device_aware_probe_pool(&streams, Some(profile), Some(max_bitrate));

        let first = pool
            .first()
            .expect("pool must not be empty");
        let first_probe_data = first
            .probe_data
            .as_ref()
            .expect("top-1 candidate must carry probe data");
        let reasons = crate::device_profile::compute_transcode_reasons(
            first_probe_data,
            Some(profile),
            api::EmbeddedSubtitleHandling::default(),
            None,
            Some(max_bitrate as i64),
        );
        assert!(
            has_no_video_reencode_reason(&reasons),
            "{label}: top-1 must not need a video re-encode; reasons={:?}",
            reasons
                .0
                .iter()
                .map(api::TranscodeReason::name)
                .collect::<Vec<_>>()
        );

        let first_filename = first
            .stream_info
            .as_ref()
            .and_then(|si| {
                si.filename
                    .as_deref()
            })
            .unwrap_or_default();
        assert!(
            !first_filename.contains("REMUX"),
            "{label}: the 52.4 GB / 68.6 Mbps remux (candidate A) must never \
             be top-1 at this cap; got {first_filename}"
        );
    }

    /// SPEC: with a profile, candidates partition into compatible-first then others; against the repo jellyfin-web fixture at a 65.6 Mbps cap, the winner needs no video re-encode and the 52.4 GB remux is never top-1.
    #[test]
    fn desired_device_aware_probe_pool_outcome_on_jellyfin_web_repo_profile() {
        assert_device_aware_pool_outcome(
            &jellyfin_web_real_profile(),
            65_573_770,
            "jellyfin-web repo fixture",
        );
    }

    /// SPEC: same as above, against the live-like jellyfin-web profile (no DOVI, no ac3/eac3).
    #[test]
    fn desired_device_aware_probe_pool_outcome_on_jellyfin_web_live_profile() {
        assert_device_aware_pool_outcome(
            &jellyfin_web_live_profile(),
            65_573_770,
            "jellyfin-web live profile",
        );
    }

    /// SPEC: rule (b) coverage — a correctly-tagged DOVI 2160p HEVC candidate that is UNDER the bitrate cap (so rule (c) alone can't disqualify it) must rank after every compatible candidate once the profile's hevc CodecProfile no longer allows DOVI (the live-like profile).
    #[test]
    fn desired_device_aware_probe_pool_orders_under_cap_dovi_after_compatible_on_live_profile()
     {
        let streams = device_aware_candidates();
        let profile = jellyfin_web_live_profile();

        let pool = device_aware_probe_pool(&streams, Some(&profile), Some(65_573_770));

        let dovi_under_cap_position = pool
            .iter()
            .position(|m| {
                m.stream_info
                    .as_ref()
                    .and_then(|si| {
                        si.filename
                            .as_deref()
                    })
                    .is_some_and(|f| f.contains("DOVIUNDERCAP"))
            })
            .expect("the under-cap DOVI candidate (D) must be present");

        // Everything genuinely compatible under the live-like profile (the
        // three 1080p H.264 candidates, probed or filename-guessed) must
        // rank before D.
        let compatible_positions: Vec<usize> = pool
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                match &m.probe_data {
                    Some(info) => info
                        .media_streams
                        .iter()
                        .any(|s| {
                            matches!(s.type_, Some(MediaStreamType::Video))
                                && s.codec
                                    .as_deref()
                                    == Some("h264")
                        }),
                    // C3: filename-guess-only 1080p H.264 candidate.
                    None => true,
                }
            })
            .map(|(i, _)| i)
            .collect();

        for pos in compatible_positions {
            assert!(
                pos < dovi_under_cap_position,
                "an under-cap DOVI candidate must rank after every \
                 compatible candidate once the profile's hevc CodecProfile \
                 no longer allows DOVI; compatible candidate at {pos}, DOVI \
                 candidate at {dovi_under_cap_position}"
            );
        }
    }

    /// SPEC: with the Infuse-like profile (genuinely mkv/HEVC/DoVi/TrueHD capable) and a 200 Mbps cap, the first element is the 2160p remux — unchanged from today's pre-probe behaviour.
    #[test]
    fn desired_device_aware_probe_pool_keeps_remux_first_on_infuse_like_profile() {
        let streams = device_aware_candidates();
        let profile = infuse_like_dovi_profile();

        let pool = device_aware_probe_pool(&streams, Some(&profile), Some(200_000_000));

        let first_filename = pool
            .first()
            .and_then(|m| {
                m.stream_info
                    .as_ref()
                    .and_then(|si| {
                        si.filename
                            .as_deref()
                    })
            })
            .unwrap_or_default();
        assert!(
            first_filename.contains("REMUX"),
            "an Infuse-like profile can direct-play/cheap-remux the 4K \
             release, so it must stay first; got {first_filename}"
        );
    }

    /// Builds a fresh `StreamService` with `profile`/`max_bitrate` set, runs
    /// the auto-play branch, and asserts the same "honest, profile-agnostic"
    /// outcome as `assert_device_aware_pool_outcome` — plus that the served
    /// candidate really is `device_aware_probe_pool(...)[0]`.
    fn assert_auto_play_outcome(
        ctx: &AppContext,
        profile: api::DeviceProfile,
        max_bitrate: u64,
        label: &str,
    ) {
        let item_id = Uuid::new_v4();
        let streams = device_aware_candidates();

        let mut service = StreamService::new(StreamServiceConfig {
            ctx: ctx.clone(),
            item_id,
            requested_id: Some(item_id),
            show_ungrouped: false,
            stream_filter: None,
            user_id: None,
        });
        service.streams = streams.clone();
        service.device_profile = Some(profile.clone());
        service.max_bitrate = Some(max_bitrate);

        let sel = service.select_streams();
        assert_eq!(
            sel.candidates
                .len(),
            1
        );
        let served = &sel.candidates[0];

        let expected_first =
            device_aware_probe_pool(&streams, Some(&profile), Some(max_bitrate))
                .into_iter()
                .next()
                .expect("pool must not be empty");
        assert_eq!(
            served.id, expected_first.id,
            "{label}: auto-play with a device profile present must serve \
             device_aware_probe_pool(...)[0], not all_streams[0]"
        );

        let served_probe_data = served
            .probe_data
            .as_ref()
            .expect("served candidate must carry probe data");
        let reasons = crate::device_profile::compute_transcode_reasons(
            served_probe_data,
            Some(&profile),
            api::EmbeddedSubtitleHandling::default(),
            None,
            Some(max_bitrate as i64),
        );
        assert!(
            has_no_video_reencode_reason(&reasons),
            "{label}: served candidate must not need a video re-encode; \
             reasons={:?}",
            reasons
                .0
                .iter()
                .map(api::TranscodeReason::name)
                .collect::<Vec<_>>()
        );

        let served_filename = served
            .stream_info
            .as_ref()
            .and_then(|si| {
                si.filename
                    .as_deref()
            })
            .unwrap_or_default();
        assert!(
            !served_filename.contains("REMUX"),
            "{label}: the 52.4 GB / 68.6 Mbps remux (candidate A) must never \
             be served at this cap; got {served_filename}"
        );
    }

    /// SPEC: same outcome rule as S2/S4/S5 — auto-play with a device profile must serve `device_aware_probe_pool(...)[0]`, that candidate must not need a video re-encode, and candidate A must never be served at a 65.6 Mbps cap. Repo jellyfin-web fixture.
    #[tokio::test]
    async fn desired_auto_play_outcome_on_jellyfin_web_repo_profile() {
        use crate::integration_test::authenticated_server;

        let (_server, guard, _token) = authenticated_server().await;
        assert_auto_play_outcome(
            &guard.0,
            jellyfin_web_real_profile(),
            65_573_770,
            "jellyfin-web repo fixture",
        );
    }

    /// SPEC: same as above, against the live-like jellyfin-web profile (no DOVI, no ac3/eac3).
    #[tokio::test]
    async fn desired_auto_play_outcome_on_jellyfin_web_live_profile() {
        use crate::integration_test::authenticated_server;

        let (_server, guard, _token) = authenticated_server().await;
        assert_auto_play_outcome(
            &guard.0,
            jellyfin_web_live_profile(),
            65_573_770,
            "jellyfin-web live profile",
        );
    }

    /// SPEC: with the live-like profile and a 20 Mbps cap — where every 2160p candidate in the fixture exceeds the cap (asserted below) — auto-play must serve a 1080p H.264 candidate.
    #[tokio::test]
    async fn desired_auto_play_serves_1080p_h264_on_live_profile_20mbps_cap() {
        use crate::integration_test::authenticated_server;

        let (_server, guard, _token) = authenticated_server().await;
        let ctx = &guard.0;
        let item_id = Uuid::new_v4();
        let streams = device_aware_candidates();

        // Precondition: every 2160p candidate in this fixture exceeds the 20
        // Mbps cap, so "1080p wins" follows necessarily.
        for stream in &streams {
            let Some(info) = stream
                .probe_data
                .as_ref()
            else {
                continue;
            };
            let is_2160p = info
                .media_streams
                .iter()
                .any(|s| {
                    matches!(s.type_, Some(MediaStreamType::Video))
                        && s.height == Some(2160)
                });
            if is_2160p {
                assert!(
                    info.bitrate
                        .unwrap_or(0)
                        > 20_000_000,
                    "precondition: every 2160p candidate must exceed the 20 \
                     Mbps cap"
                );
            }
        }

        let mut service = StreamService::new(StreamServiceConfig {
            ctx: ctx.clone(),
            item_id,
            requested_id: Some(item_id),
            show_ungrouped: false,
            stream_filter: None,
            user_id: None,
        });
        service.streams = streams;
        service.device_profile = Some(jellyfin_web_live_profile());
        service.max_bitrate = Some(20_000_000);

        let sel = service.select_streams();
        assert_eq!(
            sel.candidates
                .len(),
            1
        );
        let served = &sel.candidates[0];
        let served_video = served
            .probe_data
            .as_ref()
            .and_then(|info| {
                info.media_streams
                    .iter()
                    .find(|s| matches!(s.type_, Some(MediaStreamType::Video)))
            })
            .expect("served candidate must carry probe data");
        assert_eq!(
            served_video
                .codec
                .as_deref(),
            Some("h264")
        );
        assert_eq!(
            (served_video.width, served_video.height),
            (Some(1920), Some(1080))
        );
    }

    /// SPEC: with the Infuse-like profile set on the service, auto-play must still serve the 2160p remux.
    #[tokio::test]
    async fn desired_auto_play_uses_device_aware_selection_with_infuse_like_profile() {
        use crate::integration_test::authenticated_server;

        let (_server, guard, _token) = authenticated_server().await;
        let ctx = &guard.0;
        let item_id = Uuid::new_v4();

        let streams = device_aware_candidates();

        let mut service = StreamService::new(StreamServiceConfig {
            ctx: ctx.clone(),
            item_id,
            requested_id: Some(item_id),
            show_ungrouped: false,
            stream_filter: None,
            user_id: None,
        });
        service.streams = streams.clone();
        service.device_profile = Some(infuse_like_dovi_profile());
        service.max_bitrate = Some(200_000_000);

        let sel = service.select_streams();
        assert_eq!(
            sel.candidates
                .len(),
            1
        );
        let served = &sel.candidates[0];
        let served_filename = served
            .stream_info
            .as_ref()
            .and_then(|si| {
                si.filename
                    .as_deref()
            })
            .unwrap_or_default();
        assert!(
            served_filename.contains("REMUX"),
            "an Infuse-like profile must still get served the 2160p remux; \
             got {served_filename}"
        );
    }
}
