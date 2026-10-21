-- Distinguish server-generated artwork from user-uploaded / provider artwork.
-- Only generated rows may ever be regenerated, invalidated or reshaped.
ALTER TABLE media_images ADD COLUMN is_generated INTEGER NOT NULL DEFAULT 0;

-- Safe backfill: mark a collection/folder Primary as generated only when that is
-- provable. Everything else stays 0 (= treated as uploaded, never touched).
--   * The collection has an image config: with a config, an uploaded Primary is
--     stored as the Backdrop, so its Primary can only be generated.
--   * Or the row carries the old generator's exact signature: a 960x540 JPEG
--     written as meta/library/<id>/primary.jpg.
UPDATE media_images
SET is_generated = 1
WHERE image_type = 'primary'
  AND image_index = 0
  AND media_id IN (SELECT id FROM media WHERE kind IN ('collection', 'folder'))
  AND (
        media_id IN (
            SELECT id FROM media
            WHERE collection_image_config IS NOT NULL
              AND collection_image_config <> 'null'
        )
        OR (
            width = 960 AND height = 540
            AND path LIKE '%/meta/library/%/primary.jpg'
        )
      );
