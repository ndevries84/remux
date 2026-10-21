use crate::{
    api::{
        CollectionType, DeviceProfile, GetItemsQuery, ItemSortBy, SortOrder,
        SubtitleDeliveryMethod, SubtitleProfile,
    },
    db::auth::Device,
    device_profile::SubtitleCodec,
};

pub trait JellyfinClient: Send {
    /// Additional subtitle capabilities for this client. The client's reported
    /// profile is kept intact; these entries are merged into it per request.
    fn device_profile_subtitles(&self) -> Option<Vec<SubtitleProfile>> {
        None
    }

    fn hide_sources(&self) -> bool {
        false
    }

    /// Whether this client wants generated collection art as a portrait (2:3)
    /// Primary. Every other client keeps the classic wide (16:9) Primary.
    fn prefers_portrait_collection_art(&self) -> bool {
        false
    }

    fn mixed_collection_type(&self) -> Option<CollectionType> {
        None
    }

    /// Returns true when the query carries the client's built-in default sort,
    /// meaning the user has not expressed a preference and the collection's own
    /// sort order should be applied instead.
    fn is_default_sort(&self, q: &GetItemsQuery) -> bool {
        q.sort_by
            .as_deref()
            .map(|s| {
                s.is_empty()
                    || matches!(
                        s.first(),
                        Some(
                            ItemSortBy::SortName
                                | ItemSortBy::Name
                                | ItemSortBy::IsFolder
                                | ItemSortBy::DateCreated
                                | ItemSortBy::DateLastContentAdded
                        )
                    )
            })
            .unwrap_or(true)
    }
}

pub struct Plezy;
pub struct Moonfin;
pub struct Swiftfin;
pub struct SenPlayer;
pub struct Infuse;
pub struct GenericClient;

impl JellyfinClient for Plezy {
    fn hide_sources(&self) -> bool {
        true
    }
}

impl JellyfinClient for Moonfin {
    fn prefers_portrait_collection_art(&self) -> bool {
        true
    }
}

impl JellyfinClient for Swiftfin {
    fn mixed_collection_type(&self) -> Option<CollectionType> {
        // Swiftfin's SDK has no "mixed" case; homevideos is accepted and shows a home row.
        Some(CollectionType::Homevideos)
    }
}

impl JellyfinClient for SenPlayer {
    fn is_default_sort(&self, q: &GetItemsQuery) -> bool {
        // SenPlayer's built-in default: DateLastContentAdded,DateCreated,SortName / Descending.
        let is_senplayer_default = q
            .sort_by
            .as_deref()
            == Some(&[
                ItemSortBy::DateLastContentAdded,
                ItemSortBy::DateCreated,
                ItemSortBy::SortName,
            ])
            && q.sort_order
                .as_deref()
                == Some(&[SortOrder::Descending]);

        is_senplayer_default
            || q.sort_by
                .as_deref()
                .map(|s| {
                    s.is_empty()
                        || matches!(
                            s.first(),
                            Some(
                                ItemSortBy::SortName
                                    | ItemSortBy::Name
                                    | ItemSortBy::IsFolder
                                    | ItemSortBy::DateCreated
                                    | ItemSortBy::DateLastContentAdded
                            )
                        )
                })
                .unwrap_or(true)
    }
}

impl JellyfinClient for Infuse {
    fn device_profile_subtitles(&self) -> Option<Vec<SubtitleProfile>> {
        // Infuse can read these tracks inside the original video even though
        // its Jellyfin profile advertises only external subtitle delivery.
        Some(
            [
                SubtitleCodec::Pgs,
                SubtitleCodec::Srt,
                SubtitleCodec::Ass,
                SubtitleCodec::DvdSub,
                SubtitleCodec::DvbSub,
                SubtitleCodec::WebVtt,
                SubtitleCodec::MovText,
            ]
            .into_iter()
            .map(|format| SubtitleProfile {
                format: Some(format.to_string()),
                method: Some(SubtitleDeliveryMethod::Embed),
            })
            .collect(),
        )
    }
}

impl JellyfinClient for GenericClient {}

pub fn from_device(device: &Device) -> Box<dyn JellyfinClient> {
    match device
        .app_name
        .as_str()
    {
        "Plezy" => Box::new(Plezy),
        // Moonfin's Flutter app reports "Moonfin for <platform>" (tvOS,
        // Android TV, Android, iOS, macOS, Windows, Linux, Web), "Moonfin on
        // Github" (standalone web build) or a bare "Moonfin". The Moonfin
        // plugin's UI inside jellyfin-web reports jellyfin-web's own name and
        // is therefore treated as a stock client.
        s if s
            .to_ascii_lowercase()
            .starts_with("moonfin") =>
        {
            Box::new(Moonfin)
        }
        s if s.contains("Swiftfin") => Box::new(Swiftfin),
        "SenPlayer" => Box::new(SenPlayer),
        s if s
            .to_ascii_lowercase()
            .starts_with("infuse") =>
        {
            Box::new(Infuse)
        }
        _ => Box::new(GenericClient),
    }
}

pub fn merge_device_profile_subtitles(
    device: &Device,
    profile: Option<DeviceProfile>,
) -> Option<DeviceProfile> {
    let mut profile = profile?;
    if let Some(additional) = from_device(device).device_profile_subtitles() {
        for subtitle in additional {
            if !profile
                .subtitle_profiles
                .iter()
                .any(|existing| {
                    existing.method == subtitle.method
                        && existing
                            .format
                            .as_deref()
                            .zip(
                                subtitle
                                    .format
                                    .as_deref(),
                            )
                            .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
                })
            {
                profile
                    .subtitle_profiles
                    .push(subtitle);
            }
        }
    }
    Some(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_moonfin_clients_prefer_portrait_collection_art() {
        let prefers = |app_name: &str| {
            from_device(&Device {
                app_name: app_name.into(),
                ..Default::default()
            })
            .prefers_portrait_collection_art()
        };
        // Names Moonfin sends (moonfin-core lib/di/injection.dart `_clientName`).
        for moonfin in [
            "Moonfin for tvOS",
            "Moonfin for Android TV",
            "Moonfin for Android",
            "Moonfin for iOS",
            "Moonfin for macOS",
            "Moonfin for Windows",
            "Moonfin for Linux",
            "Moonfin for Web",
            "Moonfin on Github",
            "Moonfin",
            "moonfin",
        ] {
            assert!(prefers(moonfin), "{moonfin} should prefer portrait art");
        }
        for other in [
            "Jellyfin Web",
            "Jellyfin Android",
            "Swiftfin",
            "Swiftfin iOS",
            "Infuse-Direct",
            "SenPlayer",
            "Plezy",
            "Generic",
            "Unknown",
            "",
        ] {
            assert!(!prefers(other), "{other:?} must keep wide collection art");
        }
    }

    #[test]
    fn date_created_sort_counts_as_default_sort() {
        let q = |sort_by| GetItemsQuery {
            sort_by,
            ..Default::default()
        };
        assert!(GenericClient.is_default_sort(&q(Some(vec![ItemSortBy::DateCreated]))));
        assert!(
            GenericClient
                .is_default_sort(&q(Some(vec![ItemSortBy::DateLastContentAdded])))
        );
        assert!(GenericClient.is_default_sort(&q(Some(vec![ItemSortBy::SortName]))));
        assert!(
            !GenericClient.is_default_sort(&q(Some(vec![ItemSortBy::CommunityRating])))
        );
    }

    #[test]
    fn infuse_adds_embed_without_replacing_reported_subtitle_profiles() {
        let device = Device {
            app_name: "Infuse-Direct".into(),
            ..Default::default()
        };
        let reported = DeviceProfile {
            max_streaming_bitrate: Some(200_000_000),
            subtitle_profiles: vec![SubtitleProfile {
                format: Some("vtt".into()),
                method: Some(SubtitleDeliveryMethod::External),
            }],
            ..Default::default()
        };

        let merged =
            merge_device_profile_subtitles(&device, Some(reported.clone())).unwrap();
        assert_eq!(merged.max_streaming_bitrate, reported.max_streaming_bitrate);
        assert!(
            merged
                .subtitle_profiles
                .iter()
                .any(|profile| {
                    profile
                        .format
                        .as_deref()
                        == Some("vtt")
                        && profile.method == Some(SubtitleDeliveryMethod::External)
                })
        );
        for codec in ["pgssub", "subrip", "ass"] {
            assert!(merged.subtitle_profiles.iter().any(|profile| {
                profile.method == Some(SubtitleDeliveryMethod::Embed)
                    && profile.format.as_deref().is_some_and(|format| {
                        crate::device_profile::subtitle_codec_matches_profile(codec, format)
                    })
            }));
        }
        assert_eq!(
            reported
                .subtitle_profiles
                .len(),
            1
        );

        let merged_again =
            merge_device_profile_subtitles(&device, Some(merged.clone())).unwrap();
        assert_eq!(
            merged_again
                .subtitle_profiles
                .len(),
            merged
                .subtitle_profiles
                .len()
        );
    }

    #[test]
    fn generic_client_does_not_change_profile_and_missing_profile_stays_missing() {
        let generic = Device {
            app_name: "Generic".into(),
            ..Default::default()
        };
        let profile = DeviceProfile {
            subtitle_profiles: vec![SubtitleProfile {
                format: Some("vtt".into()),
                method: Some(SubtitleDeliveryMethod::External),
            }],
            ..Default::default()
        };
        let merged = merge_device_profile_subtitles(&generic, Some(profile)).unwrap();
        assert_eq!(
            merged
                .subtitle_profiles
                .len(),
            1
        );

        let infuse = Device {
            app_name: "Infuse".into(),
            ..Default::default()
        };
        assert!(merge_device_profile_subtitles(&infuse, None).is_none());
    }
}
