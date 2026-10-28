//! The asset store — port of the Python `assets.py`.
//!
//! Images live on disk under the data dir as `<id><suffix>` plus a
//! `<id>.json` sidecar record (same field names as the Python store),
//! referenced as `asset://<id>`. Two hashes per asset: `sha256` of the
//! stored file bytes and `pixel_sha256` of the decoded RGBA bytes,
//! plus `color_profile_sha256` of the embedded ICC profile when one is
//! present. Lineage (`source_asset_id`) chains every derived asset
//! back to its source photo; draw refuses a foreground whose chain
//! does not end at a source photo.

use crate::image::{
    decode_image, encode_png_gray, encode_png_rgba, sha256_hex, GrayImage, RgbaImage,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Prefix of every asset handle (`asset://<id>`).
pub const HANDLE_PREFIX: &str = "asset://";

/// Asset kind: an ingested source photo.
pub const KIND_SOURCE: &str = "source_photo";
/// Asset kind: a cut-out foreground PNG.
pub const KIND_FOREGROUND: &str = "foreground_png";
/// Asset kind: a segmentation mask PNG.
pub const KIND_MASK: &str = "mask_png";
/// Asset kind: a generated background plate.
pub const KIND_PLATE: &str = "background_plate";
/// Asset kind: a composited (staged) image.
pub const KIND_COMPOSITE: &str = "composite";
/// Asset kind: a mask overlay preview.
pub const KIND_OVERLAY: &str = "mask_overlay";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AssetRecord {
    pub asset_id: String,
    pub sha256: String,
    pub kind: String,
    pub filename: String,
    pub byte_size: usize,
    pub created_at: String,
    #[serde(default)]
    pub source_asset_id: Option<String>,
    #[serde(default)]
    pub pixel_sha256: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub color_profile_sha256: Option<String>,
}

/// The on-disk asset store: image files under the data dir, indexed
/// by `assets.json`.
pub struct AssetStore {
    pub data_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub index_path: PathBuf,
}

impl AssetStore {
    pub fn new(data_dir: &Path) -> AssetStore {
        AssetStore {
            data_dir: data_dir.to_path_buf(),
            assets_dir: data_dir.join("assets"),
            index_path: data_dir.join("assets.json"),
        }
    }

    pub fn parse_handle(handle: &str) -> Result<String, String> {
        let id = handle
            .strip_prefix(HANDLE_PREFIX)
            .ok_or_else(|| format!("Not an asset handle: {handle:?}"))?;
        let id = id.trim();
        if id.is_empty() {
            return Err(format!("Empty asset id in handle: {handle:?}"));
        }
        Ok(id.to_string())
    }

    fn sidecar_path(&self, asset_id: &str) -> PathBuf {
        self.assets_dir.join(format!("{asset_id}.json"))
    }

    fn load_legacy_index(&self) -> serde_json::Map<String, serde_json::Value> {
        std::fs::read_to_string(&self.index_path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default()
    }

    pub fn load_record(&self, asset_id: &str) -> Result<AssetRecord, String> {
        let sidecar = self.sidecar_path(asset_id);
        if sidecar.exists() {
            let text = std::fs::read_to_string(&sidecar)
                .map_err(|e| format!("cannot read sidecar: {e}"))?;
            return serde_json::from_str(&text)
                .map_err(|e| format!("corrupt sidecar for {asset_id}: {e}"));
        }
        let legacy = self.load_legacy_index();
        if let Some(value) = legacy.get(asset_id) {
            return serde_json::from_value(value.clone())
                .map_err(|e| format!("corrupt legacy record for {asset_id}: {e}"));
        }
        Err(format!("Unknown asset id: {asset_id:?}"))
    }

    fn save_record(&self, record: &AssetRecord) -> Result<(), String> {
        std::fs::create_dir_all(&self.assets_dir)
            .map_err(|e| format!("cannot create assets dir: {e}"))?;
        let sidecar = self.sidecar_path(&record.asset_id);
        let tmp = self
            .assets_dir
            .join(format!("{}.json.tmp-{}", record.asset_id, uuid_hex()));
        let text = serde_json::to_string_pretty(record).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("cannot write sidecar: {e}"))?;
        std::fs::rename(&tmp, &sidecar).map_err(|e| format!("cannot commit sidecar: {e}"))?;
        Ok(())
    }

    /// (pixel_sha256, width, height, color_profile_sha256) for image
    /// bytes; all None when the data does not decode as an image.
    fn image_facts(data: &[u8]) -> (Option<String>, Option<u32>, Option<u32>, Option<String>) {
        match decode_image(data) {
            Ok(decoded) => {
                let rgba = decoded.into_rgba();
                (
                    Some(rgba.pixel_sha256()),
                    Some(rgba.width),
                    Some(rgba.height),
                    rgba.color_profile_sha256(),
                )
            }
            Err(_) => (None, None, None, None),
        }
    }

    pub fn put_bytes(
        &self,
        data: &[u8],
        kind: &str,
        suffix: &str,
        source_asset_id: Option<&str>,
    ) -> Result<String, String> {
        std::fs::create_dir_all(&self.assets_dir)
            .map_err(|e| format!("cannot create assets dir: {e}"))?;
        let asset_id = uuid_hex();
        let filename = format!("{asset_id}{suffix}");
        let tmp = self
            .assets_dir
            .join(format!("{filename}.tmp-{}", uuid_hex()));
        std::fs::write(&tmp, data).map_err(|e| format!("cannot write asset: {e}"))?;
        std::fs::rename(&tmp, self.assets_dir.join(&filename))
            .map_err(|e| format!("cannot commit asset: {e}"))?;
        let (pixel_sha256, width, height, color_profile_sha256) = Self::image_facts(data);
        let record = AssetRecord {
            asset_id: asset_id.clone(),
            sha256: sha256_hex(data),
            kind: kind.to_string(),
            filename,
            byte_size: data.len(),
            created_at: crate::ledger::utc_now_iso(),
            source_asset_id: source_asset_id.map(|s| s.to_string()),
            pixel_sha256,
            width,
            height,
            color_profile_sha256,
        };
        self.save_record(&record)?;
        Ok(HANDLE_PREFIX.to_string() + &asset_id)
    }

    /// Store an RGBA image as a PNG asset (its ICC profile embedded).
    pub fn put_image(
        &self,
        image: &RgbaImage,
        kind: &str,
        source_asset_id: Option<&str>,
    ) -> Result<String, String> {
        let bytes = encode_png_rgba(image);
        self.put_bytes(&bytes, kind, ".png", source_asset_id)
    }

    pub fn put_gray_image(
        &self,
        image: &GrayImage,
        kind: &str,
        source_asset_id: Option<&str>,
    ) -> Result<String, String> {
        let bytes = encode_png_gray(image);
        self.put_bytes(&bytes, kind, ".png", source_asset_id)
    }

    pub fn get_record(&self, handle: &str) -> Result<AssetRecord, String> {
        self.load_record(&Self::parse_handle(handle)?)
    }

    /// Where an asset's persisted fidelity receipt lives, if any:
    /// beside the asset's data file, named `<asset_id>.receipt.json`.
    pub fn receipt_path(&self, handle: &str) -> Result<PathBuf, String> {
        let asset_id = Self::parse_handle(handle)?;
        Ok(self.assets_dir.join(format!("{asset_id}.receipt.json")))
    }

    /// The asset's persisted fidelity receipt, or None if none exists.
    pub fn load_receipt(&self, handle: &str) -> Result<Option<serde_json::Value>, String> {
        let path = self.receipt_path(handle)?;
        if !path.exists() {
            return Ok(None);
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("cannot read receipt: {e}"))?;
        let value = serde_json::from_str(&text)
            .map_err(|e| format!("corrupt receipt beside {handle}: {e}"))?;
        Ok(Some(value))
    }

    /// Persist a draw receipt beside its composite asset as
    /// `<asset_id>.receipt.json` (the Python store's `save_receipt`).
    pub fn save_receipt(&self, handle: &str, receipt: &serde_json::Value) -> Result<(), String> {
        let asset_id = Self::parse_handle(handle)?;
        std::fs::create_dir_all(&self.assets_dir)
            .map_err(|e| format!("cannot create assets dir: {e}"))?;
        let path = self.receipt_path(handle)?;
        let tmp = self
            .assets_dir
            .join(format!("{asset_id}.receipt.json.tmp-{}", uuid_hex()));
        let text = serde_json::to_string_pretty(receipt).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, text).map_err(|e| format!("cannot write receipt: {e}"))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("cannot commit receipt: {e}"))?;
        Ok(())
    }

    pub fn resolve(&self, handle: &str) -> Result<PathBuf, String> {
        let record = self.get_record(handle)?;
        Ok(self.assets_dir.join(&record.filename))
    }

    pub fn read_bytes(&self, handle: &str) -> Result<Vec<u8>, String> {
        let path = self.resolve(handle)?;
        std::fs::read(&path).map_err(|e| format!("cannot read asset {handle}: {e}"))
    }

    /// Decode the asset as RGBA, carrying its ICC profile.
    pub fn read_image(&self, handle: &str) -> Result<RgbaImage, String> {
        let data = self.read_bytes(handle)?;
        decode_image(&data)
            .map(|d| d.into_rgba())
            .map_err(|e| format!("asset {handle} does not decode: {e}"))
    }

    pub fn read_gray_image(&self, handle: &str) -> Result<GrayImage, String> {
        let data = self.read_bytes(handle)?;
        match decode_image(&data)? {
            crate::image::DecodedImage::Gray(g) => Ok(g),
            crate::image::DecodedImage::Rgba(rgba) => {
                // PIL: convert("L") uses ITU-R 601-2 luma.
                let mut pixels = Vec::with_capacity((rgba.width * rgba.height) as usize);
                for px in rgba.pixels.chunks_exact(4) {
                    let l = (px[0] as u32 * 19595
                        + px[1] as u32 * 38470
                        + px[2] as u32 * 7471
                        + 0x8000)
                        >> 16;
                    pixels.push(l as u8);
                }
                Ok(GrayImage {
                    width: rgba.width,
                    height: rgba.height,
                    pixels,
                    icc_profile: rgba.icc_profile,
                })
            }
        }
    }

    pub fn lineage(&self, handle: &str) -> Result<Vec<AssetRecord>, String> {
        let mut chain = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut record = self.get_record(handle)?;
        loop {
            if !seen.insert(record.asset_id.clone()) {
                return Err(format!("Cycle in asset lineage at {:?}.", record.asset_id));
            }
            let next = record.source_asset_id.clone();
            chain.push(record);
            match next {
                None => return Ok(chain),
                Some(parent) => record = self.load_record(&parent)?,
            }
        }
    }

    /// Re-hash the stored asset; Err on any mismatch (file bytes,
    /// decoded pixels, colour profile).
    pub fn verify_integrity(&self, handle: &str) -> Result<(), String> {
        let record = self.get_record(handle)?;
        let data = self.read_bytes(handle)?;
        let actual_file = sha256_hex(&data);
        if actual_file != record.sha256 {
            return Err(format!(
                "Asset {handle:?} file hash mismatch: recorded {}, actual {actual_file}. The stored bytes were modified after ingest.",
                record.sha256
            ));
        }
        if record.pixel_sha256.is_some() || record.color_profile_sha256.is_some() {
            let (pixel_sha256, _, _, color_profile_sha256) = Self::image_facts(&data);
            if let Some(expected) = &record.pixel_sha256 {
                if pixel_sha256.as_deref() != Some(expected.as_str()) {
                    return Err(format!(
                        "Asset {handle:?} pixel hash mismatch: recorded {expected}, actual {:?}. The decoded pixels differ from what was stored.",
                        pixel_sha256
                    ));
                }
            }
            if let Some(expected) = &record.color_profile_sha256 {
                if color_profile_sha256.as_deref() != Some(expected.as_str()) {
                    return Err(format!(
                        "Asset {handle:?} colour profile mismatch: recorded {expected}, actual {:?}. The stored image's colour profile was changed after ingest; identical pixel values would display as different colours.",
                        color_profile_sha256
                    ));
                }
            }
        }
        Ok(())
    }
}

fn uuid_hex() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}
