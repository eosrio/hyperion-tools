//! AtomicMarket state → eosio-contract-api-shaped MongoDB docs (the `atomicmarket` contract tables).
//!
//! The market half of the `--tables atomicmarket`/`atomic` preset. AtomicMarket listing tables hold no
//! `serialized_data`, so no schema registry is needed — these are plain field projections, with the
//! one transform being asset/symbol price parsing (`"1.0000 WAX"` → exact base units + symbol +
//! precision).
//!
//! ## Snapshot scope (S / H)
//! A snapshot holds only **live** listings. On-chain, terminal listings (sold / cancelled / declined)
//! are *deleted*, so they are simply absent — every snapshot listing is effectively in a live state
//! (`WAITING`/`LISTED`). History-only (**H**) fields — `buyer`/`final_price`/`taker_marketplace` for
//! sold sales, full auction **bid history** (the struct keeps only the latest bid), `template_mint`,
//! all `*_at_time` timestamps, and the `atomicmarket_stats_markets` trade-fact table — are **omitted**
//! and left to the live feed / ES. Every doc carries `block_num` as state provenance.

use serde_json::{json, Map, Value};

use crate::atomicassets::{parse_asset, parse_symbol_spec};
use crate::map::RowCtx;

pub const COLL_AM_SALES: &str = "atomicmarket-sales";
pub const COLL_AM_AUCTIONS: &str = "atomicmarket-auctions";
pub const COLL_AM_BUYOFFERS: &str = "atomicmarket-buyoffers";
pub const COLL_AM_TEMPLATE_BUYOFFERS: &str = "atomicmarket-tbuyoffers";
pub const COLL_AM_MARKETPLACES: &str = "atomicmarket-marketplaces";
pub const COLL_AM_CONFIG: &str = "atomicmarket-config";

/// All AtomicMarket Mongo collections (for the preset's `--mongo-drop` set + index naming).
pub const ALL_COLLECTIONS: &[&str] = &[
    COLL_AM_SALES,
    COLL_AM_AUCTIONS,
    COLL_AM_BUYOFFERS,
    COLL_AM_TEMPLATE_BUYOFFERS,
    COLL_AM_MARKETPLACES,
    COLL_AM_CONFIG,
];

/// Contract tables the `atomicmarket` preset loads.
pub const ATOMICMARKET_TABLES: &[&str] = &[
    "atomicmarket:sales",
    "atomicmarket:auctions",
    "atomicmarket:buyoffers",
    "atomicmarket:tbuyoffers",
    "atomicmarket:marketplaces",
    "atomicmarket:config",
];

// AtomicMarket listing states (matches eosio-contract-api's SaleState / AuctionState enums for the
// states a *snapshot* can represent — terminal states are absent because the rows are deleted).
const STATE_WAITING: i64 = 0;
const STATE_LISTED: i64 = 1;

// ── mappers ──────────────────────────────────────────────────────────────────────────────────────

/// `sales` row → sale doc. `listing_price` (asset) splits into base-unit `listing_price` + symbol +
/// precision; `settlement_symbol` splits into name + precision. `state` is `LISTED` once the backing
/// offer exists (`offer_id != 0`), else `WAITING`.
pub fn map_sale(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = base_market_doc(ctx, &data, &["sale_id", "seller", "asset_ids"]);

    insert_price(
        &mut doc,
        data.get("listing_price"),
        "listing_price",
        "listing_symbol",
        "listing_symbol_precision",
    );
    insert_symbol_spec(
        &mut doc,
        data.get("settlement_symbol"),
        "settlement_symbol",
        "settlement_symbol_precision",
    );

    let offer_id = data.get("offer_id").cloned().unwrap_or(Value::Null);
    let has_offer = offer_id
        .as_str()
        .and_then(|s| s.parse::<i64>().ok())
        .or_else(|| offer_id.as_i64())
        .map(|i| i > 0)
        .unwrap_or(false);
    doc.insert("offer_id".into(), offer_id);
    doc.insert(
        "state".into(),
        json!(if has_offer {
            STATE_LISTED
        } else {
            STATE_WAITING
        }),
    );

    for f in ["maker_marketplace", "collection_name", "collection_fee"] {
        if let Some(v) = data.get(f) {
            doc.insert(f.into(), v.clone());
        }
    }
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `auctions` row → auction doc. `current_bid` (asset) → base-unit `price` + token symbol/precision;
/// `current_bidder` is surfaced as both `current_bidder` and `buyer` (the API's field). `state` is
/// `LISTED` once assets are transferred in, else `WAITING`.
pub fn map_auction(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = base_market_doc(ctx, &data, &["auction_id", "seller", "asset_ids"]);

    insert_price(
        &mut doc,
        data.get("current_bid"),
        "price",
        "token_symbol",
        "token_precision",
    );

    for f in [
        "end_time",
        "assets_transferred",
        "current_bidder",
        "claimed_by_seller",
        "claimed_by_buyer",
        "maker_marketplace",
        "taker_marketplace",
        "collection_name",
        "collection_fee",
    ] {
        if let Some(v) = data.get(f) {
            doc.insert(f.into(), v.clone());
        }
    }
    // API's `buyer` == the current high bidder.
    if let Some(v) = data.get("current_bidder") {
        doc.insert("buyer".into(), v.clone());
    }
    let transferred = data
        .get("assets_transferred")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    doc.insert(
        "state".into(),
        json!(if transferred {
            STATE_LISTED
        } else {
            STATE_WAITING
        }),
    );
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `buyoffers` row → buyoffer doc. `recipient` is the prospective `seller`; `price` (asset) splits to
/// base units + symbol + precision.
pub fn map_buyoffer(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = base_market_doc(ctx, &data, &["buyoffer_id", "buyer", "asset_ids", "memo"]);
    if let Some(v) = data.get("recipient") {
        doc.insert("seller".into(), v.clone());
    }
    insert_price(
        &mut doc,
        data.get("price"),
        "price",
        "token_symbol",
        "token_precision",
    );
    for f in ["maker_marketplace", "collection_name", "collection_fee"] {
        if let Some(v) = data.get(f) {
            doc.insert(f.into(), v.clone());
        }
    }
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `tbuyoffers` (template buyoffers) row → doc. Like buyoffers but bid on a `template_id` rather than
/// explicit assets.
pub fn map_template_buyoffer(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = base_market_doc(ctx, &data, &["buyoffer_id", "buyer", "template_id"]);
    insert_price(
        &mut doc,
        data.get("price"),
        "price",
        "token_symbol",
        "token_precision",
    );
    for f in ["maker_marketplace", "collection_name", "collection_fee"] {
        if let Some(v) = data.get(f) {
            doc.insert(f.into(), v.clone());
        }
    }
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `marketplaces` row → marketplace doc.
pub fn map_marketplace(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = base_market_doc(ctx, &data, &["marketplace_name", "creator"]);
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

/// `config` singleton → config doc (fees, durations, account refs, supported tokens/pairs passed
/// through as decoded).
pub fn map_config(ctx: &RowCtx, data: Value) -> Value {
    let mut doc = Map::new();
    doc.insert("market_contract".into(), json!(ctx.code));
    if let Value::Object(m) = &data {
        for (k, v) in m {
            doc.insert(k.clone(), v.clone());
        }
    }
    doc.insert("block_num".into(), json!(ctx.block_num));
    Value::Object(doc)
}

// ── helpers ──────────────────────────────────────────────────────────────────────────────────────

/// Seed a market doc with `market_contract` and the given verbatim pass-through fields.
fn base_market_doc(ctx: &RowCtx, data: &Value, passthrough: &[&str]) -> Map<String, Value> {
    let mut doc = Map::new();
    doc.insert("market_contract".into(), json!(ctx.code));
    for f in passthrough {
        if let Some(v) = data.get(*f) {
            doc.insert((*f).into(), v.clone());
        }
    }
    doc
}

/// Split an `asset` field into `{price_field: base_units, symbol_field: SYM, precision_field: N}`.
fn insert_price(
    doc: &mut Map<String, Value>,
    asset: Option<&Value>,
    price_field: &str,
    symbol_field: &str,
    precision_field: &str,
) {
    if let Some((amount, symbol, precision)) = asset.and_then(Value::as_str).and_then(parse_asset) {
        doc.insert(price_field.into(), json!(amount));
        doc.insert(symbol_field.into(), json!(symbol));
        doc.insert(precision_field.into(), json!(precision));
    }
}

/// Split a `symbol` field ("8,WAX") into `{symbol_field: SYM, precision_field: N}`.
fn insert_symbol_spec(
    doc: &mut Map<String, Value>,
    sym: Option<&Value>,
    symbol_field: &str,
    precision_field: &str,
) {
    if let Some((symbol, precision)) = sym.and_then(Value::as_str).and_then(parse_symbol_spec) {
        doc.insert(symbol_field.into(), json!(symbol));
        doc.insert(precision_field.into(), json!(precision));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> RowCtx<'static> {
        RowCtx {
            code: "atomicmarket",
            scope: "atomicmarket",
            table: "sales",
            primary_key: "1".into(),
            payer: "atomicmarket",
            block_num: 99,
            token_ok: false,
            lightapi_fields: false,
        }
    }

    #[test]
    fn map_sale_splits_price_and_derives_state() {
        let data = json!({
            "sale_id": "12345",
            "seller": "alice",
            "asset_ids": ["1099512961542"],
            "offer_id": "777",
            "listing_price": "10.50000000 WAX",
            "settlement_symbol": "8,WAX",
            "maker_marketplace": "mymarket",
            "collection_name": "mycollection",
            "collection_fee": 0.05,
        });
        let doc = map_sale(&ctx(), data);
        assert_eq!(doc["sale_id"], json!("12345"));
        assert_eq!(doc["listing_price"], json!("1050000000"));
        assert_eq!(doc["listing_symbol"], json!("WAX"));
        assert_eq!(doc["listing_symbol_precision"], json!(8));
        assert_eq!(doc["settlement_symbol"], json!("WAX"));
        assert_eq!(doc["state"], json!(STATE_LISTED));
        assert_eq!(doc["asset_ids"][0], json!("1099512961542"));
    }

    #[test]
    fn map_sale_waiting_when_no_offer() {
        let data = json!({
            "sale_id": "1",
            "seller": "bob",
            "asset_ids": [],
            "offer_id": "0",
            "listing_price": "1.0000 EOS",
            "settlement_symbol": "4,EOS",
        });
        let doc = map_sale(&ctx(), data);
        assert_eq!(doc["state"], json!(STATE_WAITING));
    }

    #[test]
    fn map_auction_derives_state_from_assets_transferred() {
        let data = json!({
            "auction_id": "55",
            "seller": "carol",
            "asset_ids": ["1"],
            "end_time": 1_900_000_000u64,
            "assets_transferred": true,
            "current_bid": "5.00000000 WAX",
            "current_bidder": "dave",
            "claimed_by_seller": false,
            "claimed_by_buyer": false,
        });
        let doc = map_auction(&ctx(), data);
        assert_eq!(doc["price"], json!("500000000"));
        assert_eq!(doc["token_symbol"], json!("WAX"));
        assert_eq!(doc["buyer"], json!("dave"));
        assert_eq!(doc["state"], json!(STATE_LISTED));
    }
}
