//! Game Pass catalog: titles, artwork.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::check;
use crate::error::Result;

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct CatalogImage {
    #[serde(rename = "URL", default)]
    pub url: String,
    #[serde(rename = "Width", default)]
    pub width: Option<u32>,
    #[serde(rename = "Height", default)]
    pub height: Option<u32>,
}

impl CatalogImage {
    pub fn https_url(&self) -> Option<String> {
        if self.url.is_empty() {
            None
        } else if self.url.starts_with("//") {
            Some(format!("https:{}", self.url))
        } else {
            Some(self.url.clone())
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "PascalCase")]
pub struct Product {
    #[serde(default)]
    pub store_id: String,
    #[serde(default)]
    pub product_title: String,
    #[serde(default)]
    pub publisher_name: Option<String>,
    #[serde(rename = "XCloudTitleId", default)]
    pub xcloud_title_id: Option<String>,
    #[serde(rename = "Image_Tile", default)]
    pub image_tile: Option<CatalogImage>,
    #[serde(rename = "Image_Poster", default)]
    pub image_poster: Option<CatalogImage>,
    #[serde(default)]
    pub short_description: Option<String>,
}

impl Product {
    /// Square-ish artwork, preferring the tile.
    pub fn tile_url(&self) -> Option<String> {
        self.image_tile
            .as_ref()
            .and_then(CatalogImage::https_url)
            .or_else(|| self.image_poster.as_ref().and_then(CatalogImage::https_url))
    }

    pub fn poster_url(&self) -> Option<String> {
        self.image_poster
            .as_ref()
            .and_then(CatalogImage::https_url)
            .or_else(|| self.tile_url())
    }
}

#[derive(Debug, Deserialize)]
struct ProductsResponse {
    #[serde(rename = "Products", default)]
    products: HashMap<String, Product>,
}

/// Resolve product ids to catalog entries (batched by 100).
pub async fn products(
    http: &reqwest::Client,
    market: &str,
    language: &str,
    product_ids: &[String],
) -> Result<HashMap<String, Product>> {
    let mut out = HashMap::new();
    let market = if market.is_empty() { "US" } else { market };
    for chunk in product_ids.chunks(100) {
        let url = format!(
            "https://catalog.gamepass.com/v3/products?market={market}&language={language}&hydration=RemoteHighSapphire0"
        );
        let resp = http
            .post(url)
            .header("ms-cv", "0")
            .header("calling-app-name", "Xbox Cloud Gaming Web")
            .header("calling-app-version", "26.1.97")
            .json(&json!({ "Products": chunk }))
            .send()
            .await?;
        let r: ProductsResponse = check(resp).await?.json().await?;
        for (id, mut p) in r.products {
            if p.store_id.is_empty() {
                p.store_id = id.clone();
            }
            out.insert(id, p);
        }
    }
    Ok(out)
}
